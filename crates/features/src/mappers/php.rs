//! PHP mapper: composer.json bins + scripts, PSR-4 autoload roots,
//! `**/*.phpt` test fixtures (php-src convention), `ext/<name>/config.{m4,w32}`
//! extension slices (php-src), and Laravel `routes/{web,api,console,channels}.php`
//! route extraction. Covers framework-agnostic Composer, php-src internals,
//! and Laravel without per-app config.

use std::fs;
use std::path::Path;
use std::sync::OnceLock;

use anyhow::Result;
use codesage_protocol::{FeatureConfidence, FeatureKind, Language, Reference, ReferenceKind};
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
        let composer = read_composer(root);
        if let Some(c) = &composer {
            seeds.extend(composer_seeds(root, c));
            seeds.extend(composer_script_seeds(c));
        }
        seeds.extend(php_src_extensions(root)?);
        seeds.extend(php_extension_at_root(root)?);
        let routes = parse_laravel_routes(root)?;
        seeds.extend(laravel_route_seeds(&routes));
        // Laravel application-layer slices: controllers + the expanded
        // surface from clawpatch PR #5. These add coarser feature_ids
        // than per-route registrations so agents can ask
        // "what slice owns app/Jobs/SendInvoice.php?" cleanly.
        if is_laravel_project(root, composer.as_ref()) {
            seeds.extend(laravel_project_seed(root, composer.as_ref())?);
            seeds.extend(laravel_controllers(root, &routes)?);
            seeds.extend(laravel_form_requests(root)?);
            seeds.extend(laravel_artisan_commands(root)?);
            seeds.extend(laravel_jobs(root)?);
            seeds.extend(laravel_services(root)?);
            seeds.extend(laravel_models(root)?);
            seeds.extend(laravel_migrations(root)?);
            seeds.extend(laravel_seeders(root)?);
            seeds.extend(laravel_test_suites(root)?);
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
            // A PSR-4 namespace may map to multiple directories; register each.
            let paths: Vec<String> = match path_val {
                Value::String(s) => vec![s.clone()],
                Value::Array(arr) => arr
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect(),
                _ => continue,
            };
            let ns_clean = ns.trim_end_matches('\\');
            for path in paths {
                if path.is_empty() {
                    continue;
                }
                let entry = path.trim_end_matches('/').to_string();
                let abs = root.join(&entry);
                if !is_safe_dir(root, &abs) {
                    continue;
                }
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
        let api_context = php_src_api_context(root, &ext_rel, &name, config_basename);
        out.extend(php_extension_api_seeds(
            root,
            &p,
            api_context,
            vec![format!("{ext_rel}/tests")],
        ));
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

#[derive(Debug, Clone)]
struct PhpApiEntry {
    source: &'static str,
    title: String,
    summary: String,
    entry_path: String,
    entry_symbol: String,
    tags: Vec<String>,
}

fn php_extension_api_seeds(
    root: &Path,
    dir: &Path,
    context_files: Vec<SeedFile>,
    test_prefixes: Vec<String>,
) -> Vec<FeatureSeed> {
    php_extension_api_entries(root, dir)
        .into_iter()
        .map(|entry| FeatureSeed {
            title: entry.title,
            summary: entry.summary,
            kind: FeatureKind::Library,
            source: entry.source,
            confidence: FeatureConfidence::High,
            entry_path: entry.entry_path,
            entry_symbol: Some(entry.entry_symbol),
            entry_route: None,
            entry_command: None,
            test_command: None,
            language: Language::Php,
            tags: entry.tags,
            owned_files: Vec::new(),
            context_files: context_files.clone(),
            tests: Vec::new(),
            test_prefixes: test_prefixes.clone(),
        })
        .collect()
}

fn php_extension_api_entries(root: &Path, dir: &Path) -> Vec<PhpApiEntry> {
    let mut out = Vec::new();
    for source_path in php_extension_api_source_files(root, dir) {
        out.extend(php_api_entries_in_file(root, &source_path));
    }
    out
}

fn php_extension_api_source_files(root: &Path, dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        let Some(fname) = p.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(stem) = fname
            .strip_suffix(".cpp")
            .or_else(|| fname.strip_suffix(".cc"))
            .or_else(|| fname.strip_suffix(".c"))
        else {
            continue;
        };
        if stem.ends_with("_arginfo") {
            continue;
        }
        if is_safe_file(root, &p) {
            out.push(rel_path(root, &p));
        }
    }
    out.sort();
    out
}

fn php_api_entries_in_file(root: &Path, entry_path: &str) -> Vec<PhpApiEntry> {
    let path = root.join(entry_path);
    let Ok(raw) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let code = strip_c_comments_and_strings(&strip_if_zero_blocks(&raw));
    let mut out = Vec::new();
    for caps in php_api_macro_re().captures_iter(&code) {
        match &caps[1] {
            "METHOD" => {
                let Some(method) = caps.get(3) else {
                    continue;
                };
                let class = caps[2].to_string();
                let method = method.as_str().to_string();
                let symbol = format!("{class}::{method}");
                out.push(PhpApiEntry {
                    source: "php-ext-method",
                    title: format!("PHP method `{symbol}`"),
                    summary: format!(
                        "PHP_METHOD({class}, {method}) implementation in {entry_path}"
                    ),
                    entry_path: entry_path.to_string(),
                    entry_symbol: symbol,
                    tags: vec![
                        "php".to_string(),
                        "php-extension".to_string(),
                        "method".to_string(),
                    ],
                });
            }
            "FUNCTION" => {
                if caps.get(3).is_some() {
                    continue;
                }
                let function = caps[2].to_string();
                out.push(PhpApiEntry {
                    source: "php-ext-function",
                    title: format!("PHP function `{function}`"),
                    summary: format!("PHP_FUNCTION({function}) implementation in {entry_path}"),
                    entry_path: entry_path.to_string(),
                    entry_symbol: function,
                    tags: vec![
                        "php".to_string(),
                        "php-extension".to_string(),
                        "function".to_string(),
                    ],
                });
            }
            _ => {}
        }
    }
    out
}

fn php_api_macro_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"\bPHP_(METHOD|FUNCTION)\s*\(\s*([A-Za-z_][A-Za-z0-9_]*)\s*(?:,\s*([A-Za-z_][A-Za-z0-9_]*))?\s*\)",
        )
        .expect("valid PHP extension API macro regex")
    })
}

/// Blank out `#if 0` … `#endif` regions before macro scanning. php-src
/// routinely parks removed handlers in inactive preprocessor blocks.
fn strip_if_zero_blocks(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_if_zero = false;
    let mut depth = 0;

    for line in input.split_inclusive('\n') {
        let trimmed = line.trim();
        if !in_if_zero {
            if is_if_zero_directive(trimmed) {
                in_if_zero = true;
                depth = 1;
                out.push_str(&blank_line_preserve_newline(line));
            } else {
                out.push_str(line);
            }
            continue;
        }

        out.push_str(&blank_line_preserve_newline(line));
        if (trimmed.starts_with("#if")
            && !trimmed.starts_with("#ifdef")
            && !trimmed.starts_with("#ifndef"))
            || trimmed.starts_with("#ifdef")
            || trimmed.starts_with("#ifndef")
        {
            depth += 1;
        } else if trimmed == "#endif" || trimmed.starts_with("#endif ") {
            depth -= 1;
            if depth == 0 {
                in_if_zero = false;
            }
        } else if depth == 1
            && (trimmed.starts_with("#elif") || trimmed == "#else" || trimmed.starts_with("#else "))
        {
            // The `#if 0` arm is dead; a top-level `#elif`/`#else` switches to
            // an active branch, so stop blanking from here. fnd: CR-039.
            in_if_zero = false;
        }
    }

    out
}

fn is_if_zero_directive(trimmed: &str) -> bool {
    if !trimmed.starts_with("#if") {
        return false;
    }
    let rest = trimmed
        .trim_start_matches("#if")
        .trim_start_matches(|c: char| c.is_whitespace());
    rest.starts_with('0')
        && matches!(
            rest.chars().nth(1),
            None | Some(' ') | Some('\t') | Some('/') | Some('*')
        )
}

fn blank_line_preserve_newline(line: &str) -> String {
    line.chars()
        .map(|c| if c == '\n' { '\n' } else { ' ' })
        .collect()
}

fn strip_c_comments_and_strings(input: &str) -> String {
    enum State {
        Code,
        LineComment,
        BlockComment,
        DoubleString,
        SingleString,
    }

    let mut out = String::with_capacity(input.len());
    let mut state = State::Code;
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match state {
            State::Code => match ch {
                '/' if chars.peek() == Some(&'/') => {
                    out.push(' ');
                    chars.next();
                    out.push(' ');
                    state = State::LineComment;
                }
                '/' if chars.peek() == Some(&'*') => {
                    out.push(' ');
                    chars.next();
                    out.push(' ');
                    state = State::BlockComment;
                }
                '"' => {
                    out.push(' ');
                    state = State::DoubleString;
                }
                '\'' => {
                    out.push(' ');
                    state = State::SingleString;
                }
                _ => {
                    out.push(ch);
                }
            },
            State::LineComment => {
                if ch == '\n' {
                    out.push('\n');
                    state = State::Code;
                } else {
                    out.push(' ');
                }
            }
            State::BlockComment => {
                if ch == '*' && chars.peek() == Some(&'/') {
                    out.push(' ');
                    chars.next();
                    out.push(' ');
                    state = State::Code;
                } else {
                    out.push(if ch == '\n' { '\n' } else { ' ' });
                }
            }
            State::DoubleString => {
                if ch == '\\' {
                    out.push(' ');
                    if let Some(next) = chars.next() {
                        out.push(if next == '\n' { '\n' } else { ' ' });
                    }
                } else if ch == '"' {
                    out.push(' ');
                    state = State::Code;
                } else {
                    out.push(if ch == '\n' { '\n' } else { ' ' });
                }
            }
            State::SingleString => {
                if ch == '\\' {
                    out.push(' ');
                    if let Some(next) = chars.next() {
                        out.push(if next == '\n' { '\n' } else { ' ' });
                    }
                } else if ch == '\'' {
                    out.push(' ');
                    state = State::Code;
                } else {
                    out.push(if ch == '\n' { '\n' } else { ' ' });
                }
            }
        }
    }
    out
}

/// Blank out PHP comments (`//`, `#`, `/* */`) but keep string contents
/// intact. The route regexes need to see the `'/path'` literal verbatim,
/// so we cannot reuse `strip_c_comments_and_strings` here. State-tracking
/// strings is still required to avoid mistaking a `//` inside a string
/// for the start of a comment. Byte offsets are preserved by replacing
/// every stripped char with a space (newlines stay so line numbers line
/// up with downstream tools that consume the same offsets).
fn strip_php_comments_preserving_strings(input: &str) -> String {
    enum State {
        Code,
        LineComment,
        BlockComment,
        DoubleString,
        SingleString,
    }

    let mut out = String::with_capacity(input.len());
    let mut state = State::Code;
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match state {
            State::Code => match ch {
                '/' if chars.peek() == Some(&'/') => {
                    out.push(' ');
                    chars.next();
                    out.push(' ');
                    state = State::LineComment;
                }
                '#' if !matches!(chars.peek(), Some(&'[')) => {
                    // PHP `#` line comment. The `#[Attribute]` syntax is
                    // attribute-not-comment; preserve it so attributes
                    // appearing on route definitions still parse.
                    out.push(' ');
                    state = State::LineComment;
                }
                '/' if chars.peek() == Some(&'*') => {
                    out.push(' ');
                    chars.next();
                    out.push(' ');
                    state = State::BlockComment;
                }
                '"' => {
                    out.push('"');
                    state = State::DoubleString;
                }
                '\'' => {
                    out.push('\'');
                    state = State::SingleString;
                }
                _ => out.push(ch),
            },
            State::LineComment => {
                if ch == '\n' {
                    out.push('\n');
                    state = State::Code;
                } else {
                    out.push(' ');
                }
            }
            State::BlockComment => {
                if ch == '*' && chars.peek() == Some(&'/') {
                    out.push(' ');
                    chars.next();
                    out.push(' ');
                    state = State::Code;
                } else {
                    out.push(if ch == '\n' { '\n' } else { ' ' });
                }
            }
            State::DoubleString => {
                out.push(ch);
                if ch == '\\' {
                    if let Some(next) = chars.next() {
                        out.push(next);
                    }
                } else if ch == '"' {
                    state = State::Code;
                }
            }
            State::SingleString => {
                out.push(ch);
                if ch == '\\' {
                    if let Some(next) = chars.next() {
                        out.push(next);
                    }
                } else if ch == '\'' {
                    state = State::Code;
                }
            }
        }
    }
    out
}

fn php_src_api_context(
    root: &Path,
    ext_rel: &str,
    name: &str,
    config_basename: &str,
) -> Vec<SeedFile> {
    let mut context_files = Vec::new();
    push_seed_file(
        root,
        &mut context_files,
        &format!("{ext_rel}/{config_basename}"),
        "extension build config",
    );
    push_seed_file(
        root,
        &mut context_files,
        &format!("{ext_rel}/php_{name}.h"),
        "extension public header",
    );
    push_seed_file(
        root,
        &mut context_files,
        &format!("{ext_rel}/{name}.h"),
        "extension header",
    );
    context_files
}

fn root_php_extension_api_context(
    root: &Path,
    ext_name: &str,
    entry_config: &str,
    header_path: &str,
) -> Vec<SeedFile> {
    let mut context_files = Vec::new();
    push_seed_file(
        root,
        &mut context_files,
        entry_config,
        "extension build config",
    );
    push_seed_file(
        root,
        &mut context_files,
        header_path,
        "extension public header",
    );
    push_seed_file(
        root,
        &mut context_files,
        &format!("{ext_name}.stub.php"),
        "PHP stub declarations",
    );
    push_seed_file(
        root,
        &mut context_files,
        "composer.json",
        "package manifest",
    );
    let alternate_config = if entry_config == "config.m4" {
        "config.w32"
    } else {
        "config.m4"
    };
    push_seed_file(
        root,
        &mut context_files,
        alternate_config,
        "extension build config",
    );
    context_files
}

fn push_seed_file(root: &Path, files: &mut Vec<SeedFile>, path: &str, reason: &str) {
    if files.iter().any(|f| f.path == path) {
        return;
    }
    if is_safe_file(root, &root.join(path)) {
        files.push(SeedFile {
            path: path.to_string(),
            reason: reason.to_string(),
        });
    }
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
    let api_context = root_php_extension_api_context(root, &ext_name, entry_config, &header_path);
    seeds.extend(php_extension_api_seeds(
        root,
        root,
        api_context,
        vec!["tests".to_string()],
    ));

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
    /// 1-based line of the route registration in `file`, derived from the
    /// regex match offset. Carried onto synthesized `RouteHandler`
    /// references so `find_references` points at the declaration.
    line: u32,
}

/// 1-based line number of `byte_off` within `text`.
fn line_at(text: &str, byte_off: usize) -> u32 {
    let off = byte_off.min(text.len());
    (text[..off].bytes().filter(|&b| b == b'\n').count() as u32) + 1
}

fn parse_laravel_routes(root: &Path) -> Result<Vec<LaravelRoute>> {
    let mut out = Vec::new();
    let routes_dir = root.join("routes");
    if !is_safe_dir(root, &routes_dir) {
        return Ok(out);
    }
    // Single regex: `Route::<chain>?<verb>('/path' [, ClassRef::class [, 'action']])`.
    // Group 1 captures the fluent chain that precedes the verb call
    // (e.g. `prefix('admin')->middleware('auth')->`) so we can pull
    // `prefix(...)` out per registration. The chain is non-greedy
    // method() segments terminated by `->`.
    let route_re = Regex::new(
        r#"(?ms)Route::((?:[A-Za-z_]\w*\s*\([^;]*?\)\s*->\s*)*)(get|post|put|patch|delete|options|any|match|resource|apiResource)\s*\(\s*(?:\[[^\]]*\]\s*,\s*)?['"]([^'"]+)['"](?:\s*,\s*(?:\[\s*)?(\\?[A-Za-z_][A-Za-z0-9_\\]*)::class(?:\s*,\s*['"]([^'"]+)['"])?)?"#,
    )?;
    // `Route::<chain>?controller(X::class)<chain>?->group(function (...) { body })`.
    // Body capture is non-greedy with `(?s)` so a stray `}` inside a
    // string literal won't truncate the body before its real close.
    let controller_group_re = Regex::new(
        r#"(?ms)Route::((?:[A-Za-z_]\w*\s*\([^;]*?\)\s*->\s*)*)controller\s*\(\s*(\\?[A-Za-z_][A-Za-z0-9_\\]*)::class\s*\)\s*->\s*((?:[A-Za-z_]\w*\s*\([^;]*?\)\s*->\s*)*)group\s*\(\s*function\s*\([^)]*\)\s*\{(.*?)\}\s*\)\s*;"#,
    )?;
    // Inner route inside a `controller(...)->group(...)` body: the
    // closing `Route::verb('/path', 'action')` shape (no class — it
    // comes from the outer controller).
    let group_inner_re = Regex::new(
        r#"(?ms)Route::((?:[A-Za-z_]\w*\s*\([^;]*?\)\s*->\s*)*)(get|post|put|patch|delete|options|any)\s*\(\s*['"]([^'"]+)['"](?:\s*,\s*['"]([^'"]+)['"])?"#,
    )?;
    // Any `Route::<chain>group(function (...) { body });`. Used to catch
    // prefix/middleware-only groups (no controller). The chain segment's
    // `[^;]*?` can't span the `;` inside the closure body, so a `group(...)`
    // call is never mistaken for a chain element and always binds as the
    // literal group opener.
    let group_re = Regex::new(
        r#"(?ms)Route::((?:[A-Za-z_]\w*\s*\([^;]*?\)\s*->\s*)*)group\s*\(\s*function\s*\([^)]*\)\s*\{(.*?)\}\s*\)\s*;"#,
    )?;
    for file in ["web.php", "api.php", "console.php", "channels.php"] {
        let path = routes_dir.join(file);
        if !is_safe_file(root, &path) {
            continue;
        }
        let rel = rel_path(root, &path);
        let raw = fs::read_to_string(&path).unwrap_or_default();
        // Strip PHP comments before the route regexes so commented-out
        // `Route::get(...)` lines don't produce phantom feature seeds.
        // Strings are preserved (route patterns are string literals);
        // byte offsets are stable so `consumed_spans` cross-pass dedupe
        // remains valid. fnd_4a2842c3.
        let scanned = strip_php_comments_preserving_strings(&raw);
        let imports = parse_php_use_imports(&raw);
        let file_prefixes = file_default_route_prefixes(file);

        // Pass 1: Route::controller(X::class)->group(fn () { … })
        // bodies. Each inner route inherits the outer controller. We
        // also remember which byte spans were consumed so the
        // top-level scan in pass 2 doesn't double-emit them.
        let mut consumed_spans: Vec<(usize, usize)> = Vec::new();
        for cap in controller_group_re.captures_iter(&scanned) {
            let span = cap.get(0).map(|m| (m.start(), m.end()));
            let outer_chain = cap.get(1).map(|m| m.as_str()).unwrap_or("");
            let raw_class = cap.get(2).map(|m| m.as_str()).unwrap_or("");
            let inner_chain = cap.get(3).map(|m| m.as_str()).unwrap_or("");
            let body = cap.get(4).map(|m| m.as_str()).unwrap_or("");
            let body_base = cap.get(4).map(|m| m.start()).unwrap_or(0);
            let controller_class = resolve_imported_class(&imports, raw_class);
            if controller_class.is_none() {
                continue;
            }
            let outer_prefixes: Vec<String> = file_prefixes
                .iter()
                .cloned()
                .chain(fluent_route_prefixes(outer_chain))
                .chain(fluent_route_prefixes(inner_chain))
                .collect();
            for inner in group_inner_re.captures_iter(body) {
                let inner_chain_local = inner.get(1).map(|m| m.as_str()).unwrap_or("");
                let verb = inner
                    .get(2)
                    .map(|m| m.as_str().to_uppercase())
                    .unwrap_or_default();
                let pattern = inner.get(3).map(|m| m.as_str()).unwrap_or("");
                let action = inner.get(4).map(|m| m.as_str().to_string());
                if verb.is_empty() || pattern.is_empty() {
                    continue;
                }
                let line = inner
                    .get(0)
                    .map(|m| line_at(&scanned, body_base + m.start()))
                    .unwrap_or(1);
                let prefixes: Vec<String> = outer_prefixes
                    .iter()
                    .cloned()
                    .chain(fluent_route_prefixes(inner_chain_local))
                    .collect();
                out.push(LaravelRoute {
                    file: rel.clone(),
                    verb,
                    pattern: route_uri_with_prefixes(&prefixes, pattern),
                    controller_class: controller_class.clone(),
                    action,
                    line,
                });
            }
            if let Some(s) = span {
                consumed_spans.push(s);
            }
        }

        // Pass 1b: prefix/middleware-only groups —
        // `Route::prefix('admin')->group(function () { … })` with no
        // controller. Inner routes must inherit the group's prefix chain;
        // without this they emit with their own (empty) chain and lose the
        // group prefix. Controller groups are handled by pass 1, so skip a
        // chain that names `controller(` and any span already consumed.
        for cap in group_re.captures_iter(&scanned) {
            let span = cap.get(0).map(|m| (m.start(), m.end()));
            if let Some((s, e)) = span
                && consumed_spans.iter().any(|(cs, ce)| s >= *cs && e <= *ce)
            {
                continue;
            }
            let outer_chain = cap.get(1).map(|m| m.as_str()).unwrap_or("");
            if outer_chain.contains("controller(") {
                continue;
            }
            let body = cap.get(2).map(|m| m.as_str()).unwrap_or("");
            let body_base = cap.get(2).map(|m| m.start()).unwrap_or(0);
            let outer_prefixes: Vec<String> = file_prefixes
                .iter()
                .cloned()
                .chain(fluent_route_prefixes(outer_chain))
                .collect();
            for inner in route_re.captures_iter(body) {
                let inner_abs = inner
                    .get(0)
                    .map(|m| (body_base + m.start(), body_base + m.end()));
                // Guard against re-emitting a route a controller-group pass
                // already consumed (nested groups).
                if let Some((s, e)) = inner_abs
                    && consumed_spans.iter().any(|(cs, ce)| s >= *cs && e <= *ce)
                {
                    continue;
                }
                let inner_chain = inner.get(1).map(|m| m.as_str()).unwrap_or("");
                let verb = inner
                    .get(2)
                    .map(|m| m.as_str().to_uppercase())
                    .unwrap_or_default();
                let pattern = inner.get(3).map(|m| m.as_str()).unwrap_or("");
                let raw_class = inner.get(4).map(|m| m.as_str()).unwrap_or("");
                let action = inner.get(5).map(|m| m.as_str().to_string());
                if verb.is_empty() || pattern.is_empty() {
                    continue;
                }
                let line = inner
                    .get(0)
                    .map(|m| line_at(&scanned, body_base + m.start()))
                    .unwrap_or(1);
                let controller_class = if raw_class.is_empty() {
                    None
                } else {
                    resolve_imported_class(&imports, raw_class)
                };
                let prefixes: Vec<String> = outer_prefixes
                    .iter()
                    .cloned()
                    .chain(fluent_route_prefixes(inner_chain))
                    .collect();
                out.push(LaravelRoute {
                    file: rel.clone(),
                    verb,
                    pattern: route_uri_with_prefixes(&prefixes, pattern),
                    controller_class,
                    action,
                    line,
                });
            }
            if let Some(s) = span {
                consumed_spans.push(s);
            }
        }

        // Pass 2: top-level `Route::<chain>?<verb>('/path' [, X::class])`
        // matches that didn't fall inside a controller-group body.
        for cap in route_re.captures_iter(&scanned) {
            if let Some(m) = cap.get(0) {
                let (start, end) = (m.start(), m.end());
                if consumed_spans.iter().any(|(s, e)| start >= *s && end <= *e) {
                    continue;
                }
            }
            let chain = cap.get(1).map(|m| m.as_str()).unwrap_or("");
            let verb = cap
                .get(2)
                .map(|m| m.as_str().to_uppercase())
                .unwrap_or_default();
            let pattern = cap.get(3).map(|m| m.as_str()).unwrap_or("");
            let raw_class = cap.get(4).map(|m| m.as_str()).unwrap_or("");
            let action = cap.get(5).map(|m| m.as_str().to_string());
            if verb.is_empty() || pattern.is_empty() {
                continue;
            }
            let line = cap
                .get(0)
                .map(|m| line_at(&scanned, m.start()))
                .unwrap_or(1);
            let controller_class = if raw_class.is_empty() {
                None
            } else {
                resolve_imported_class(&imports, raw_class)
            };
            let prefixes: Vec<String> = file_prefixes
                .iter()
                .cloned()
                .chain(fluent_route_prefixes(chain))
                .collect();
            out.push(LaravelRoute {
                file: rel.clone(),
                verb,
                pattern: route_uri_with_prefixes(&prefixes, pattern),
                controller_class,
                action,
                line,
            });
        }
    }
    Ok(out)
}

/// Parse `use Some\Class\Name;` and `use Some\Class\Name as Alias;`
/// lines into a `short_or_alias -> fqcn` map. Used by route parsing to
/// resolve `UserController::class` to the fully-qualified namespace so
/// `laravel_controllers` can bridge routes back to the class file.
fn parse_php_use_imports(source: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let re = Regex::new(
        r"(?m)^\s*use\s+([A-Za-z_\\][A-Za-z0-9_\\]*)(?:\s+as\s+([A-Za-z_][A-Za-z0-9_]*))?\s*;",
    )
    .expect("use regex must compile");
    for cap in re.captures_iter(source) {
        let Some(fqcn) = cap
            .get(1)
            .map(|m| m.as_str().trim_start_matches('\\').to_string())
        else {
            continue;
        };
        let alias = cap
            .get(2)
            .map(|m| m.as_str().to_string())
            .or_else(|| fqcn.rsplit('\\').next().map(|s| s.to_string()));
        if let Some(a) = alias {
            out.insert(a, fqcn);
        }
    }
    out
}

/// Resolve a class reference written at a route call site to its
/// fully-qualified namespace, using the `use` map when the reference
/// is a short name. Returns `None` when the input is empty.
fn resolve_imported_class(
    imports: &std::collections::HashMap<String, String>,
    raw: &str,
) -> Option<String> {
    let trimmed = raw.trim_start_matches('\\');
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.contains('\\') {
        return Some(trimmed.to_string());
    }
    // Short name → look up through use imports; fall back to the short
    // name itself so `laravel_controllers` can still match by basename.
    Some(
        imports
            .get(trimmed)
            .cloned()
            .unwrap_or_else(|| trimmed.to_string()),
    )
}

/// Pull `prefix('admin')` values from a fluent route chain like
/// `prefix('admin')->middleware('auth')->`. The chain comes from the
/// outer regex's capture group; this is the inner pass that picks out
/// just the prefix calls so the URI can be assembled.
fn fluent_route_prefixes(chain: &str) -> Vec<String> {
    let re =
        Regex::new(r#"\bprefix\s*\(\s*['"]([^'"]*)['"]\s*\)"#).expect("prefix regex must compile");
    re.captures_iter(chain)
        .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
        .filter(|p| !p.is_empty())
        .collect()
}

/// Implicit file-level prefix. Laravel's default route service provider
/// auto-prefixes routes in `routes/api.php` with `api/`; web.php and
/// the others carry no implicit prefix.
fn file_default_route_prefixes(file: &str) -> Vec<String> {
    if file == "api.php" {
        vec!["api".to_string()]
    } else {
        Vec::new()
    }
}

/// Join prefix segments + the raw URI into the final route pattern.
/// Normalizes leading/trailing slashes so `["api"] + "/users"` →
/// `"api/users"`, not `"api//users"`.
fn route_uri_with_prefixes(prefixes: &[String], uri: &str) -> String {
    let mut parts: Vec<String> = prefixes
        .iter()
        .map(|p| p.trim_matches('/').to_string())
        .filter(|p| !p.is_empty())
        .collect();
    let cleaned = uri.trim_matches('/');
    if !cleaned.is_empty() {
        parts.push(cleaned.to_string());
    }
    let joined = parts.join("/");
    if uri.starts_with('/') || joined.is_empty() {
        format!("/{joined}").trim_end_matches('/').to_string()
    } else {
        joined
    }
}

/// Synthesize `RouteHandler` references linking each Laravel route file to
/// its handler controller method, so `impact_analysis` / `find_references`
/// traverse routing. `to_name` is the handler's PHP qualified name
/// (`Namespace\Class\method`, matching how the parser stores method
/// qualified names) so the qualified-lookup path in `find_references`
/// resolves it; the cached tail (`method`) covers unqualified lookups.
///
/// Routes that register a closure or whose controller class can't be
/// resolved (`controller_class`/`action` is `None`) produce no edge.
pub(crate) fn laravel_route_handler_refs(root: &Path) -> Result<Vec<Reference>> {
    let routes = parse_laravel_routes(root)?;
    let mut refs = Vec::new();
    for r in &routes {
        let (Some(class), Some(action)) = (&r.controller_class, &r.action) else {
            continue;
        };
        refs.push(Reference {
            from_file: r.file.clone(),
            from_symbol: None,
            to_name: format!("{class}\\{action}"),
            kind: ReferenceKind::RouteHandler,
            line: r.line,
            col: 0,
        });
    }
    Ok(refs)
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
fn is_laravel_project(root: &Path, composer: Option<&Value>) -> bool {
    if is_safe_file(root, &root.join("artisan")) {
        return true;
    }
    let Some(composer) = composer else {
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

/// Composer `scripts` entries — `composer test`, `composer setup`, etc.
/// Mirrors the npm-script slice on the JS side. We only emit a slice
/// for the script names clawpatch promotes (the ones an agent would
/// run during review/fix) plus any `deploy*` entries. Skipping unknown
/// scripts keeps `list_features` clean — Composer projects routinely
/// ship a dozen one-off pipeline scripts that aren't agent-actionable.
fn composer_script_seeds(composer: &Value) -> Vec<FeatureSeed> {
    let mut out = Vec::new();
    let Some(scripts) = composer.get("scripts").and_then(|v| v.as_object()) else {
        return out;
    };
    let allow = [
        "setup",
        "dev",
        "test",
        "typecheck",
        "lint",
        "format",
        "analyse",
        "analyze",
    ];
    for (name, val) in scripts {
        if !allow.contains(&name.as_str()) && !name.starts_with("deploy") {
            continue;
        }
        let command = match val {
            Value::String(s) => s.clone(),
            Value::Array(arr) => arr
                .iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(" && "),
            _ => continue,
        };
        let kind = if name == "test" {
            FeatureKind::TestSuite
        } else {
            FeatureKind::CliCommand
        };
        let tests = if name == "test" {
            vec![SeedTest {
                path: "composer.json".to_string(),
                command: Some("composer test".to_string()),
            }]
        } else {
            Vec::new()
        };
        out.push(FeatureSeed {
            title: format!("Composer script `{name}`"),
            summary: format!("Composer script '{name}': {command}"),
            kind,
            source: "composer-script",
            confidence: FeatureConfidence::Medium,
            entry_path: "composer.json".to_string(),
            entry_symbol: Some(name.clone()),
            entry_route: None,
            entry_command: Some(name.clone()),
            test_command: None,
            language: Language::Php,
            tags: vec![
                "php".to_string(),
                "composer".to_string(),
                "script".to_string(),
            ],
            owned_files: vec![SeedFile {
                path: "composer.json".to_string(),
                reason: "composer script".to_string(),
            }],
            context_files: Vec::new(),
            tests,
            test_prefixes: Vec::new(),
        });
    }
    out
}

/// Laravel project metadata slice. Single `service` feature that
/// bundles composer.json + artisan + bootstrap/app.php + key config
/// files into one anchor — so `find_feature("composer.json")` resolves
/// to the project rather than only to the more-specific composer-bin
/// or composer-psr4 seeds.
fn laravel_project_seed(root: &Path, composer: Option<&Value>) -> Result<Vec<FeatureSeed>> {
    let mut owned: Vec<SeedFile> = Vec::new();
    for (name, reason) in [
        ("composer.json", "Laravel project metadata"),
        ("composer.lock", "Laravel project metadata"),
        ("artisan", "Artisan entrypoint"),
        ("bootstrap/app.php", "application bootstrap"),
    ] {
        if is_safe_file(root, &root.join(name)) {
            owned.push(SeedFile {
                path: name.to_string(),
                reason: reason.to_string(),
            });
        }
    }
    if owned.is_empty() {
        return Ok(Vec::new());
    }
    let project_name = composer
        .and_then(|c| c.get("name").and_then(|n| n.as_str()))
        .and_then(|s| s.split('/').next_back().map(|s| s.to_string()))
        .unwrap_or_else(|| {
            root.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("laravel-app")
                .to_string()
        });
    let mut context = Vec::new();
    for (name, reason) in [
        ("phpunit.xml", "Laravel test configuration"),
        (".env.example", "environment contract"),
        ("config/app.php", "application config"),
        ("config/database.php", "database config"),
        ("routes/web.php", "HTTP routes"),
        ("routes/api.php", "API routes"),
        ("routes/console.php", "scheduled commands"),
    ] {
        if is_safe_file(root, &root.join(name)) {
            context.push(SeedFile {
                path: name.to_string(),
                reason: reason.to_string(),
            });
        }
    }
    let entry_path = owned[0].path.clone();
    Ok(vec![FeatureSeed {
        title: format!("Laravel project `{project_name}`"),
        summary: format!(
            "Laravel project metadata in {}",
            owned
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        kind: FeatureKind::Service,
        source: "laravel-project",
        confidence: FeatureConfidence::High,
        entry_path,
        entry_symbol: Some(project_name),
        entry_route: None,
        entry_command: None,
        test_command: None,
        language: Language::Php,
        tags: vec![
            "php".to_string(),
            "framework:laravel".to_string(),
            "project".to_string(),
        ],
        owned_files: owned,
        context_files: context,
        tests: Vec::new(),
        test_prefixes: vec!["tests".to_string()],
    }])
}

/// Common shape used by Jobs / Services / Models. Each `.php` file in
/// the directory becomes a separate feature whose `entry_symbol` is the
/// PHP class short name. Source + kind + tags + summary template are
/// passed in so the three callers stay parameter-driven instead of
/// duplicating 30-line emit blocks.
fn laravel_class_dir_seeds(
    root: &Path,
    dir: &str,
    source: &'static str,
    kind: FeatureKind,
    title_prefix: &str,
    summary_prefix: &str,
    tags: &[&str],
) -> Result<Vec<FeatureSeed>> {
    let base = root.join(dir);
    if !is_safe_dir(root, &base) {
        return Ok(Vec::new());
    }
    let files = walk_php_files(root, &base, 1000);
    let mut out = Vec::with_capacity(files.len());
    for rel in files {
        let class_short = rel
            .rsplit('/')
            .next()
            .and_then(|f| f.strip_suffix(".php"))
            .unwrap_or("Class")
            .to_string();
        out.push(FeatureSeed {
            title: format!("{title_prefix} `{class_short}`"),
            summary: format!("{summary_prefix} {class_short} in {rel}"),
            kind,
            source,
            confidence: FeatureConfidence::Medium,
            entry_path: rel.clone(),
            entry_symbol: Some(class_short),
            entry_route: None,
            entry_command: None,
            test_command: None,
            language: Language::Php,
            tags: tags.iter().map(|s| s.to_string()).collect(),
            owned_files: vec![SeedFile {
                path: rel,
                reason: title_prefix.to_lowercase(),
            }],
            context_files: Vec::new(),
            tests: Vec::new(),
            test_prefixes: vec!["tests/Feature".to_string(), "tests/Unit".to_string()],
        });
    }
    Ok(out)
}

fn laravel_jobs(root: &Path) -> Result<Vec<FeatureSeed>> {
    laravel_class_dir_seeds(
        root,
        "app/Jobs",
        "laravel-job",
        FeatureKind::Job,
        "Laravel job",
        "Laravel queueable job",
        &["php", "framework:laravel", "job", "async"],
    )
}

fn laravel_services(root: &Path) -> Result<Vec<FeatureSeed>> {
    laravel_class_dir_seeds(
        root,
        "app/Services",
        "laravel-service",
        FeatureKind::Service,
        "Laravel service",
        "Laravel application service",
        &["php", "framework:laravel", "service"],
    )
}

fn laravel_models(root: &Path) -> Result<Vec<FeatureSeed>> {
    laravel_class_dir_seeds(
        root,
        "app/Models",
        "laravel-model",
        FeatureKind::Service,
        "Laravel model",
        "Laravel Eloquent model",
        &["php", "framework:laravel", "model", "eloquent", "database"],
    )
}

/// Grouped seed for `database/migrations/**/*.php`. Migrations rarely
/// stand alone — agents reason about "the migration set" as a whole.
/// One Config feature anchored on the directory keeps `list_features`
/// clean.
fn laravel_migrations(root: &Path) -> Result<Vec<FeatureSeed>> {
    laravel_grouped_dir_seed(
        root,
        "database/migrations",
        "laravel-migration",
        "Laravel migrations",
        "schema migration",
        &[
            "php",
            "framework:laravel",
            "migration",
            "database",
            "schema",
        ],
    )
}

fn laravel_seeders(root: &Path) -> Result<Vec<FeatureSeed>> {
    laravel_grouped_dir_seed(
        root,
        "database/seeders",
        "laravel-seeder",
        "Laravel seeders",
        "database seeder",
        &["php", "framework:laravel", "seeder", "database"],
    )
}

fn laravel_grouped_dir_seed(
    root: &Path,
    dir: &str,
    source: &'static str,
    title: &str,
    file_reason: &str,
    tags: &[&str],
) -> Result<Vec<FeatureSeed>> {
    let base = root.join(dir);
    if !is_safe_dir(root, &base) {
        return Ok(Vec::new());
    }
    let files = walk_php_files(root, &base, 500);
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let owned_files: Vec<SeedFile> = files
        .iter()
        .take(50)
        .map(|p| SeedFile {
            path: p.clone(),
            reason: file_reason.to_string(),
        })
        .collect();
    Ok(vec![FeatureSeed {
        title: format!("{title} (`{dir}`)"),
        summary: format!("{title} grouped from {dir} ({} files)", files.len()),
        kind: FeatureKind::Config,
        source,
        confidence: FeatureConfidence::Medium,
        entry_path: dir.to_string(),
        entry_symbol: Some(dir.to_string()),
        entry_route: None,
        entry_command: None,
        test_command: None,
        language: Language::Php,
        tags: tags.iter().map(|s| s.to_string()).collect(),
        owned_files,
        context_files: Vec::new(),
        tests: Vec::new(),
        test_prefixes: Vec::new(),
    }])
}

/// PHPUnit / Pest test-suite slices for `tests/Unit`, `tests/Feature`,
/// `tests/Integration`, `tests/Browser`. One slice per directory that
/// actually exists. Matches the Laravel convention so an agent can ask
/// "what tests exist for the Feature suite?" without grep.
fn laravel_test_suites(root: &Path) -> Result<Vec<FeatureSeed>> {
    let mut out = Vec::new();
    for suite in ["Unit", "Feature", "Integration", "Browser"] {
        let rel = format!("tests/{suite}");
        let base = root.join(&rel);
        if !is_safe_dir(root, &base) {
            continue;
        }
        let files = walk_php_files(root, &base, 500);
        if files.is_empty() {
            continue;
        }
        let owned_files: Vec<SeedFile> = files
            .iter()
            .take(50)
            .map(|p| SeedFile {
                path: p.clone(),
                reason: "test file".to_string(),
            })
            .collect();
        out.push(FeatureSeed {
            title: format!("Laravel {suite} tests"),
            summary: format!("PHPUnit / Pest {suite} suite ({} files)", files.len()),
            kind: FeatureKind::TestSuite,
            source: "laravel-test-suite",
            confidence: FeatureConfidence::High,
            entry_path: rel.clone(),
            entry_symbol: Some(format!("tests/{suite}")),
            entry_route: None,
            entry_command: None,
            test_command: Some("composer test".to_string()),
            language: Language::Php,
            tags: vec![
                "php".to_string(),
                "framework:laravel".to_string(),
                "test-suite".to_string(),
                suite.to_lowercase(),
            ],
            owned_files,
            context_files: Vec::new(),
            tests: Vec::new(),
            test_prefixes: Vec::new(),
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
                    // Skip dependency / VCS trees even when a caller points us
                    // at a directory that contains them. fnd: CR-021.
                    let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                    if matches!(name, "vendor" | "node_modules" | ".git") {
                        continue;
                    }
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
    fn laravel_route_ignores_commented_out_routes() {
        // Regression for fnd_4a2842c3: PHP comments around `Route::*`
        // calls used to slip through the route regex and produce phantom
        // laravel-route seeds. Cover `//`, `#`, and `/* */` since all
        // three are valid PHP comment forms.
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "routes/web.php",
            r#"<?php
// Route::get('/old-home', fn() => 'home');
# Route::post('/old-login', [LoginController::class, 'store']);
/*
Route::delete('/never', fn() => null);
*/
Route::put('/api/profile', [ProfileController::class, 'update']);
"#,
        );
        let seeds = PhpMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let routes: Vec<&str> = seeds
            .iter()
            .filter(|s| s.source == "laravel-route")
            .filter_map(|s| s.entry_route.as_deref())
            .collect();
        assert!(
            routes.contains(&"PUT /api/profile"),
            "real route dropped after comment stripping: {routes:?}"
        );
        for ghost in ["GET /old-home", "POST /old-login", "DELETE /never"] {
            assert!(
                !routes.contains(&ghost),
                "commented-out route leaked through: {ghost} in {routes:?}"
            );
        }
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
    fn php_ext_root_emits_api_method_and_function_seeds() {
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
        write(
            dir.path(),
            "clickhouse.stub.php",
            "<?php class ClickHouse { public function select(string $sql): array {} }\n",
        );
        write(
            dir.path(),
            "clickhouse.cpp",
            r#"
                PHP_METHOD(ClickHouse, __construct) {}
                PHP_METHOD(ClickHouse, select) {}
                /* PHP_METHOD(ClickHouse, commentedOut) */
                // PHP_FUNCTION(clickhouse_line_comment)
                const char *example = "PHP_FUNCTION(clickhouse_in_string)";
                PHP_FUNCTION(clickhouse_escape) {}
            "#,
        );
        let seeds = PhpMapper.map(&MapperContext::for_root(dir.path())).unwrap();

        let methods: Vec<&FeatureSeed> = seeds
            .iter()
            .filter(|s| s.source == "php-ext-method")
            .collect();
        let method_symbols: Vec<&str> = methods
            .iter()
            .filter_map(|s| s.entry_symbol.as_deref())
            .collect();
        assert_eq!(
            method_symbols,
            vec!["ClickHouse::__construct", "ClickHouse::select"]
        );
        assert!(methods.iter().all(|s| s.entry_path == "clickhouse.cpp"));
        assert!(methods.iter().all(|s| s.kind == FeatureKind::Library));
        assert!(methods.iter().all(|s| s.language == Language::Php));
        assert!(
            methods.iter().all(|s| s
                .context_files
                .iter()
                .any(|f| f.path == "clickhouse.stub.php")),
            "method seeds should carry stub context"
        );
        assert!(
            !method_symbols.contains(&"ClickHouse::commentedOut"),
            "commented PHP_METHOD must not emit a feature"
        );

        let functions: Vec<&FeatureSeed> = seeds
            .iter()
            .filter(|s| s.source == "php-ext-function")
            .collect();
        assert_eq!(functions.len(), 1);
        assert_eq!(
            functions[0].entry_symbol.as_deref(),
            Some("clickhouse_escape")
        );
        assert_eq!(functions[0].entry_path, "clickhouse.cpp");
    }

    #[test]
    fn php_ext_skips_php_function_inside_if_zero() {
        let dir = tempdir().unwrap();
        write(dir.path(), "ext/demo/config.m4", "PHP_ARG_ENABLE(demo,,)\n");
        write(
            dir.path(),
            "ext/demo/demo.c",
            r#"
                PHP_FUNCTION(demo_active) {}
                #if 0
                PHP_FUNCTION(demo_removed) {}
                #endif
                #if 0 // disabled during refactor
                PHP_FUNCTION(demo_commented) {}
                #endif
            "#,
        );
        let seeds = PhpMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let symbols: Vec<&str> = seeds
            .iter()
            .filter(|s| s.source == "php-ext-function")
            .filter_map(|s| s.entry_symbol.as_deref())
            .collect();
        assert_eq!(symbols, vec!["demo_active"]);
    }

    #[test]
    fn php_src_ext_emits_api_function_seeds() {
        let dir = tempdir().unwrap();
        write(dir.path(), "ext/demo/config.m4", "PHP_ARG_ENABLE(demo,,)\n");
        write(
            dir.path(),
            "ext/demo/demo.c",
            r#"
                PHP_FUNCTION(demo_hello) {}
                PHP_METHOD(Demo, run) {}
            "#,
        );
        let seeds = PhpMapper.map(&MapperContext::for_root(dir.path())).unwrap();

        let symbols: Vec<&str> = seeds
            .iter()
            .filter(|s| matches!(s.source, "php-ext-function" | "php-ext-method"))
            .filter_map(|s| s.entry_symbol.as_deref())
            .collect();
        assert_eq!(symbols, vec!["demo_hello", "Demo::run"]);
        let api_seeds: Vec<&FeatureSeed> = seeds
            .iter()
            .filter(|s| matches!(s.source, "php-ext-function" | "php-ext-method"))
            .collect();
        assert!(api_seeds.iter().all(|s| s.entry_path == "ext/demo/demo.c"));
        assert!(api_seeds.iter().all(|s| s.language == Language::Php));
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

    // ---------- Laravel slice expansion (clawpatch PR #5) ----------

    fn laravel_composer() -> &'static str {
        r#"{"name":"acme/app","require":{"laravel/framework":"^11.0"},"scripts":{"test":"phpunit","lint":"phpcs","deploy-prod":"deploy.sh","unknown":"x"}}"#
    }

    #[test]
    fn composer_scripts_emit_allowlisted_only() {
        let dir = tempdir().unwrap();
        write(dir.path(), "composer.json", laravel_composer());
        let seeds = PhpMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let scripts: Vec<&str> = seeds
            .iter()
            .filter(|s| s.source == "composer-script")
            .filter_map(|s| s.entry_command.as_deref())
            .collect();
        assert!(scripts.contains(&"test"), "test missing in {scripts:?}");
        assert!(scripts.contains(&"lint"), "lint missing");
        assert!(
            scripts.contains(&"deploy-prod"),
            "deploy-prod missing (deploy* prefix)"
        );
        assert!(
            !scripts.contains(&"unknown"),
            "non-allowlisted 'unknown' leaked"
        );
        // The `test` script gets TestSuite kind, not CliCommand.
        let test_seed = seeds
            .iter()
            .find(|s| s.source == "composer-script" && s.entry_command.as_deref() == Some("test"))
            .unwrap();
        assert_eq!(test_seed.kind, FeatureKind::TestSuite);
    }

    #[test]
    fn laravel_project_seed_emits_for_laravel_app() {
        let dir = tempdir().unwrap();
        write(dir.path(), "composer.json", laravel_composer());
        write(dir.path(), "artisan", "#!/usr/bin/env php\n");
        write(dir.path(), "bootstrap/app.php", "<?php return null;");
        let seeds = PhpMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "laravel-project")
            .expect("laravel-project seed");
        assert_eq!(s.kind, FeatureKind::Service);
        let owned_paths: std::collections::BTreeSet<&str> =
            s.owned_files.iter().map(|f| f.path.as_str()).collect();
        assert!(owned_paths.contains("composer.json"));
        assert!(owned_paths.contains("artisan"));
        assert!(owned_paths.contains("bootstrap/app.php"));
        assert_eq!(s.entry_symbol.as_deref(), Some("app"));
    }

    #[test]
    fn laravel_jobs_models_services_emit_per_class() {
        let dir = tempdir().unwrap();
        write(dir.path(), "composer.json", laravel_composer());
        write(dir.path(), "artisan", "");
        write(
            dir.path(),
            "app/Jobs/SendInvoice.php",
            "<?php namespace App\\Jobs; class SendInvoice {}",
        );
        write(
            dir.path(),
            "app/Services/Billing.php",
            "<?php namespace App\\Services; class Billing {}",
        );
        write(
            dir.path(),
            "app/Models/User.php",
            "<?php namespace App\\Models; class User extends \\Illuminate\\Database\\Eloquent\\Model {}",
        );
        let seeds = PhpMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let by_source: std::collections::BTreeMap<&str, &FeatureSeed> = seeds
            .iter()
            .filter(|s| {
                matches!(
                    s.source,
                    "laravel-job" | "laravel-service" | "laravel-model"
                )
            })
            .map(|s| (s.source, s))
            .collect();
        let job = by_source.get("laravel-job").expect("job seed");
        assert_eq!(job.kind, FeatureKind::Job);
        assert_eq!(job.entry_symbol.as_deref(), Some("SendInvoice"));
        let svc = by_source.get("laravel-service").expect("service seed");
        assert_eq!(svc.entry_symbol.as_deref(), Some("Billing"));
        let model = by_source.get("laravel-model").expect("model seed");
        assert_eq!(model.entry_symbol.as_deref(), Some("User"));
    }

    #[test]
    fn laravel_migrations_grouped_into_one_config_seed() {
        let dir = tempdir().unwrap();
        write(dir.path(), "composer.json", laravel_composer());
        write(dir.path(), "artisan", "");
        write(
            dir.path(),
            "database/migrations/2024_01_01_create_users.php",
            "<?php return new class {};",
        );
        write(
            dir.path(),
            "database/migrations/2024_01_02_add_email.php",
            "<?php return new class {};",
        );
        let seeds = PhpMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let mig: Vec<&FeatureSeed> = seeds
            .iter()
            .filter(|s| s.source == "laravel-migration")
            .collect();
        assert_eq!(
            mig.len(),
            1,
            "expected one grouped migration seed, got {}",
            mig.len()
        );
        assert_eq!(mig[0].kind, FeatureKind::Config);
        assert_eq!(mig[0].owned_files.len(), 2);
    }

    #[test]
    fn laravel_test_suites_per_directory() {
        let dir = tempdir().unwrap();
        write(dir.path(), "composer.json", laravel_composer());
        write(dir.path(), "artisan", "");
        write(
            dir.path(),
            "tests/Unit/UserTest.php",
            "<?php class UserTest {}",
        );
        write(
            dir.path(),
            "tests/Feature/AuthTest.php",
            "<?php class AuthTest {}",
        );
        let seeds = PhpMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let suites: std::collections::BTreeSet<&str> = seeds
            .iter()
            .filter(|s| s.source == "laravel-test-suite")
            .map(|s| s.entry_path.as_str())
            .collect();
        assert!(suites.contains("tests/Unit"));
        assert!(suites.contains("tests/Feature"));
        for s in seeds.iter().filter(|s| s.source == "laravel-test-suite") {
            assert_eq!(s.kind, FeatureKind::TestSuite);
            assert_eq!(s.test_command.as_deref(), Some("composer test"));
        }
    }

    #[test]
    fn non_laravel_project_skips_application_layer_seeds() {
        // Pure Composer package (no laravel/framework dep, no artisan
        // file) must not emit laravel-project / -job / -service / etc.
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "composer.json",
            r#"{"name":"acme/pkg","autoload":{"psr-4":{"Acme\\":"src/"}}}"#,
        );
        write(dir.path(), "src/Foo.php", "<?php class Foo {}");
        // Even with an app/Jobs directory present, no jobs should emit.
        write(dir.path(), "app/Jobs/Sneaky.php", "<?php class Sneaky {}");
        let seeds = PhpMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        for src in [
            "laravel-project",
            "laravel-job",
            "laravel-service",
            "laravel-model",
            "laravel-migration",
            "laravel-seeder",
            "laravel-test-suite",
        ] {
            assert!(
                !seeds.iter().any(|s| s.source == src),
                "{src} leaked into non-Laravel project"
            );
        }
    }

    // ---------- Laravel route parser hardening (clawpatch PR #5) ----------

    fn laravel_marker(dir: &Path) {
        write(
            dir,
            "composer.json",
            r#"{"require":{"laravel/framework":"^11.0"}}"#,
        );
        write(dir, "artisan", "");
    }

    #[test]
    fn laravel_route_prefix_chain_expands_into_uri() {
        let dir = tempdir().unwrap();
        laravel_marker(dir.path());
        write(
            dir.path(),
            "routes/web.php",
            r#"<?php
use App\Http\Controllers\AdminController;
Route::prefix('admin')->get('/users', [AdminController::class, 'index']);
Route::prefix('admin')->prefix('settings')->get('/general', [AdminController::class, 'general']);
"#,
        );
        let seeds = PhpMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let routes: Vec<&str> = seeds
            .iter()
            .filter(|s| s.source == "laravel-route")
            .filter_map(|s| s.entry_route.as_deref())
            .collect();
        assert!(
            routes.iter().any(|r| r == &"GET /admin/users"),
            "prefix not expanded: {routes:?}"
        );
        assert!(
            routes.iter().any(|r| r == &"GET /admin/settings/general"),
            "nested prefix not expanded: {routes:?}"
        );
    }

    #[test]
    fn laravel_prefix_group_inherits_prefix_on_inner_routes() {
        let dir = tempdir().unwrap();
        laravel_marker(dir.path());
        write(
            dir.path(),
            "routes/web.php",
            r#"<?php
use App\Http\Controllers\UserController;
Route::prefix('admin')->group(function () {
    Route::get('/users', [UserController::class, 'index']);
    Route::post('/users', [UserController::class, 'store']);
});
"#,
        );
        let seeds = PhpMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let routes: Vec<&str> = seeds
            .iter()
            .filter(|s| s.source == "laravel-route")
            .filter_map(|s| s.entry_route.as_deref())
            .collect();
        assert!(
            routes.iter().any(|r| r == &"GET /admin/users"),
            "prefix-group prefix not inherited by inner GET: {routes:?}"
        );
        assert!(
            routes.iter().any(|r| r == &"POST /admin/users"),
            "prefix-group prefix not inherited by inner POST: {routes:?}"
        );
        // No duplicate, prefix-less emission from the top-level pass.
        assert!(
            !routes.iter().any(|r| r == &"GET /users"),
            "inner route double-emitted without prefix: {routes:?}"
        );
    }

    #[test]
    fn laravel_api_php_gets_implicit_api_prefix() {
        let dir = tempdir().unwrap();
        laravel_marker(dir.path());
        write(
            dir.path(),
            "routes/api.php",
            r#"<?php
use App\Http\Controllers\UserController;
Route::get('/users', [UserController::class, 'index']);
"#,
        );
        let seeds = PhpMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let routes: Vec<&str> = seeds
            .iter()
            .filter(|s| s.source == "laravel-route")
            .filter_map(|s| s.entry_route.as_deref())
            .collect();
        assert!(
            routes.iter().any(|r| r == &"GET /api/users"),
            "api.php route not auto-prefixed with /api: {routes:?}"
        );
    }

    #[test]
    fn laravel_use_import_resolves_short_controller_class() {
        let dir = tempdir().unwrap();
        laravel_marker(dir.path());
        write(
            dir.path(),
            "routes/web.php",
            r#"<?php
use App\Http\Controllers\UserController;
Route::get('/users', [UserController::class, 'index']);
"#,
        );
        write(
            dir.path(),
            "app/Http/Controllers/UserController.php",
            "<?php\nnamespace App\\Http\\Controllers;\nclass UserController {}",
        );
        let seeds = PhpMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        // The controller seed should pick up the route via the FQCN
        // resolved from the use import.
        let ctrl = seeds
            .iter()
            .find(|s| s.source == "laravel-controller")
            .expect("controller seed");
        assert!(
            ctrl.summary.contains("1 routes") || ctrl.entry_route.is_some(),
            "controller didn't bridge to route through use import: {}",
            ctrl.summary
        );
    }

    #[test]
    fn laravel_controller_group_body_routes_bridge_to_controller() {
        let dir = tempdir().unwrap();
        laravel_marker(dir.path());
        write(
            dir.path(),
            "routes/web.php",
            r#"<?php
use App\Http\Controllers\PostController;
Route::controller(PostController::class)->group(function () {
    Route::get('/posts', 'index');
    Route::post('/posts', 'store');
});
"#,
        );
        let seeds = PhpMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let routes: Vec<(&str, &str)> = seeds
            .iter()
            .filter(|s| s.source == "laravel-route")
            .filter_map(|s| s.entry_route.as_deref().map(|r| (r, s.summary.as_str())))
            .collect();
        // Both inner routes should surface.
        assert!(
            routes.iter().any(|(r, _)| r == &"GET /posts"),
            "GET /posts from group body missing: {routes:?}"
        );
        assert!(
            routes.iter().any(|(r, _)| r == &"POST /posts"),
            "POST /posts from group body missing: {routes:?}"
        );
        // And neither should double-emit at the top level (the consumed-span
        // tracking should suppress a second pass).
        let count_posts = routes.iter().filter(|(r, _)| r == &"GET /posts").count();
        assert_eq!(
            count_posts, 1,
            "GET /posts emitted twice (consumed-span tracking failed): {routes:?}"
        );
    }

    #[test]
    fn laravel_controller_group_with_prefix_chain() {
        let dir = tempdir().unwrap();
        laravel_marker(dir.path());
        write(
            dir.path(),
            "routes/api.php",
            r#"<?php
use App\Http\Controllers\PostController;
Route::prefix('v1')->controller(PostController::class)->group(function () {
    Route::get('/posts', 'index');
});
"#,
        );
        let seeds = PhpMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let routes: Vec<&str> = seeds
            .iter()
            .filter(|s| s.source == "laravel-route")
            .filter_map(|s| s.entry_route.as_deref())
            .collect();
        assert!(
            routes.iter().any(|r| r == &"GET /api/v1/posts"),
            "/api + /v1 + /posts not assembled: {routes:?}"
        );
    }

    #[test]
    fn laravel_use_alias_resolves() {
        let dir = tempdir().unwrap();
        laravel_marker(dir.path());
        write(
            dir.path(),
            "routes/web.php",
            r#"<?php
use App\Http\Controllers\UserController as Users;
Route::get('/users', [Users::class, 'index']);
"#,
        );
        write(
            dir.path(),
            "app/Http/Controllers/UserController.php",
            "<?php\nnamespace App\\Http\\Controllers;\nclass UserController {}",
        );
        let seeds = PhpMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        // The controller seed for UserController should now own the
        // route (the alias resolves through the use map).
        let ctrl = seeds
            .iter()
            .find(|s| {
                s.source == "laravel-controller" && s.entry_path.ends_with("UserController.php")
            })
            .expect("UserController seed");
        assert!(
            ctrl.summary.contains("1 routes"),
            "UserController didn't get its alias-resolved route: {}",
            ctrl.summary
        );
    }
}
