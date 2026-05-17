//! C / C++ mapper: detects `main()` functions via tree-sitter, plus
//! `bin_PROGRAMS` + `lib_LTLIBRARIES` from autotools `Makefile.am` and
//! `add_executable` + `add_library` from CMake. Headers in `include/` and
//! sibling `*.h` files get pulled into the corresponding library feature
//! as context.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::Path;

use anyhow::Result;
use codesage_protocol::{FeatureConfidence, FeatureKind, Language};
use regex::Regex;
use tree_sitter::Parser;

use crate::mappers::shared::{is_safe_file, walk_files};
use crate::mappers::types::{FeatureMapper, FeatureSeed, MapperContext, SeedFile};

/// Single mapper that emits both C and C++ seeds; the language tag is
/// chosen per-file based on extension. Run only once per repo, regardless
/// of whether the repo is "C" or "C++" or mixed.
pub struct CCppMapper;

impl FeatureMapper for CCppMapper {
    fn name(&self) -> &'static str {
        "c-cpp"
    }
    fn map(&self, ctx: &MapperContext) -> Result<Vec<FeatureSeed>> {
        let root = ctx.root;
        // Skip work if the repo has no C/C++ files at all.
        let files: Vec<String> = walk_files(root, root, 50_000, ctx.excludes)
            .into_iter()
            .filter(|p| is_c_or_cpp_source(p) || is_makefile(p) || is_cmake(p))
            .collect();
        if files.is_empty() {
            return Ok(Vec::new());
        }
        let mut seeds: Vec<FeatureSeed> = Vec::new();
        seeds.extend(autotools_targets(ctx, &files)?);
        seeds.extend(cmake_targets(ctx, &files)?);
        // Build-target seeds win over generic main() detection: skip
        // main()-anchored seeds for paths a CMake/autotools target
        // already claims, otherwise the same binary shows up twice with
        // different `source` tags.
        let already_seeded_paths: BTreeSet<String> = seeds
            .iter()
            .filter(|s| s.kind == FeatureKind::CliCommand)
            .map(|s| s.entry_path.clone())
            .collect();
        seeds.extend(main_function_targets(ctx, &files, &already_seeded_paths)?);
        Ok(dedup_by_entry(seeds))
    }
}

fn is_c_or_cpp_source(rel: &str) -> bool {
    rel.ends_with(".c")
        || rel.ends_with(".cpp")
        || rel.ends_with(".cc")
        || rel.ends_with(".cxx")
        || rel.ends_with(".h")
        || rel.ends_with(".hpp")
        || rel.ends_with(".hh")
        || rel.ends_with(".hxx")
}

fn is_c_or_cpp_compilable(rel: &str) -> bool {
    rel.ends_with(".c") || rel.ends_with(".cpp") || rel.ends_with(".cc") || rel.ends_with(".cxx")
}

fn is_makefile(rel: &str) -> bool {
    rel.ends_with("Makefile.am") || rel.ends_with("Makefile.in")
}

fn is_cmake(rel: &str) -> bool {
    rel.ends_with("CMakeLists.txt") || rel.ends_with(".cmake")
}

fn lang_for_path(rel: &str) -> Language {
    if rel.ends_with(".cpp")
        || rel.ends_with(".cc")
        || rel.ends_with(".cxx")
        || rel.ends_with(".hpp")
        || rel.ends_with(".hh")
        || rel.ends_with(".hxx")
    {
        Language::Cpp
    } else {
        Language::C
    }
}

fn autotools_targets(ctx: &MapperContext, files: &[String]) -> Result<Vec<FeatureSeed>> {
    let root = ctx.root;
    let mut out: Vec<FeatureSeed> = Vec::new();
    let makefile_am_files: Vec<&String> = files
        .iter()
        .filter(|f| f.ends_with("Makefile.am"))
        .collect();
    if makefile_am_files.is_empty() {
        return Ok(out);
    }
    let bin_re = Regex::new(r"(?m)^\s*bin_PROGRAMS\s*=\s*(.+)$")?;
    let lib_re = Regex::new(r"(?m)^\s*lib_LTLIBRARIES\s*=\s*(.+)$")?;
    let sources_re_template = r"(?m)^\s*{NAME}_SOURCES\s*=\s*(.+)$";
    for mf in makefile_am_files {
        let path = root.join(mf);
        let body = fs::read_to_string(&path).unwrap_or_default();
        // Join continuation lines (autoconf uses trailing backslashes).
        let body = collapse_backslash_continuations(&body);
        let dir = parent_dir(mf);
        for cap in bin_re.captures_iter(&body) {
            for name in cap
                .get(1)
                .map(|m| m.as_str())
                .unwrap_or_default()
                .split_whitespace()
            {
                if !is_valid_target_name(name) {
                    continue;
                }
                let sources = read_target_sources(&body, name, sources_re_template);
                let entry_candidates = sources
                    .iter()
                    .filter(|s| is_c_or_cpp_compilable(s))
                    .cloned()
                    .collect::<Vec<_>>();
                let entry = pick_entry(root, &dir, &entry_candidates, name).unwrap_or_else(|| {
                    format!(
                        "{}/{}",
                        dir.trim_end_matches('/'),
                        mf.rsplit('/').next().unwrap_or(mf)
                    )
                });
                if !ctx.allowed(&entry) {
                    continue;
                }
                let language = lang_for_path(&entry);
                let owned_files = filter_target_sources(ctx, &dir, &sources);
                let context_files = filter_target_context(ctx, mf, "build target declaration");
                out.push(FeatureSeed {
                    title: format!("Autotools binary `{name}`"),
                    summary: format!("Makefile.am `bin_PROGRAMS` target declared in {mf}"),
                    kind: FeatureKind::CliCommand,
                    source: "autotools-bin",
                    confidence: FeatureConfidence::High,
                    entry_path: entry,
                    entry_symbol: Some("main".to_string()),
                    entry_route: None,
                    entry_command: Some(name.to_string()),
                    test_command: None,
                    language,
                    tags: vec![
                        if language == Language::Cpp {
                            "cpp"
                        } else {
                            "c"
                        }
                        .to_string(),
                        "cli".to_string(),
                    ],
                    owned_files,
                    context_files,
                    tests: Vec::new(),
                    test_prefixes: vec![format!("{}tests", dir)],
                });
            }
        }
        for cap in lib_re.captures_iter(&body) {
            for raw in cap
                .get(1)
                .map(|m| m.as_str())
                .unwrap_or_default()
                .split_whitespace()
            {
                if !is_valid_target_name(raw) {
                    continue;
                }
                let name = raw.trim_end_matches(".la").to_string();
                let sources =
                    read_target_sources(&body, &raw.replace('.', "_"), sources_re_template);
                let entry_candidates = sources
                    .iter()
                    .filter(|s| is_c_or_cpp_compilable(s))
                    .cloned()
                    .collect::<Vec<_>>();
                let entry = pick_entry(root, &dir, &entry_candidates, &name)
                    .unwrap_or_else(|| mf.to_string());
                if !ctx.allowed(&entry) {
                    continue;
                }
                let language = lang_for_path(&entry);
                let owned_files = filter_target_sources(ctx, &dir, &sources);
                let context_files = filter_target_context(ctx, mf, "build target declaration");
                out.push(FeatureSeed {
                    title: format!("Autotools library `{name}`"),
                    summary: format!("Makefile.am `lib_LTLIBRARIES` target declared in {mf}"),
                    kind: FeatureKind::Library,
                    source: "autotools-lib",
                    confidence: FeatureConfidence::High,
                    entry_path: entry,
                    entry_symbol: None,
                    entry_route: None,
                    entry_command: None,
                    test_command: None,
                    language,
                    tags: vec![
                        if language == Language::Cpp {
                            "cpp"
                        } else {
                            "c"
                        }
                        .to_string(),
                        "library".to_string(),
                    ],
                    owned_files,
                    context_files,
                    tests: Vec::new(),
                    test_prefixes: vec![format!("{}tests", dir)],
                });
            }
        }
    }
    Ok(out)
}

fn cmake_targets(ctx: &MapperContext, files: &[String]) -> Result<Vec<FeatureSeed>> {
    let root = ctx.root;
    let mut out: Vec<FeatureSeed> = Vec::new();
    let cmake_files: Vec<&String> = files.iter().filter(|f| is_cmake(f)).collect();
    if cmake_files.is_empty() {
        return Ok(out);
    }
    // Case-insensitive (CMake commands are): ADD_EXECUTABLE, Add_Library, etc.
    // Names allow leading digits and dots (`7zip`, `foo.bar`). Source list is
    // optional so `add_executable(name)` paired with a later `target_sources()`
    // call still surfaces as a target.
    let exe_re =
        Regex::new(r"(?msi)add_executable\s*\(\s*([A-Za-z0-9_][\w\.\-]*)(?:\s+([^)]*))?\)")?;
    let lib_re = Regex::new(
        r"(?msi)add_library\s*\(\s*([A-Za-z0-9_][\w\.\-]*)(?:\s+(?:SHARED|STATIC|MODULE|OBJECT|INTERFACE))?(?:\s+([^)]*))?\)",
    )?;
    let ts_re = Regex::new(
        r"(?msi)target_sources\s*\(\s*([A-Za-z0-9_][\w\.\-]*)\s+(?:PRIVATE|PUBLIC|INTERFACE)\s+([^)]*)\)",
    )?;
    for cm in cmake_files {
        let path = root.join(cm);
        let raw = fs::read_to_string(&path).unwrap_or_default();
        let body = strip_cmake_comments(&raw);
        let dir = parent_dir(cm);

        // Collect late-bound `target_sources(name PRIVATE|PUBLIC|INTERFACE …)`
        // additions first so the target loop below can merge them when it
        // sees the matching `add_executable` / `add_library`.
        let mut extra_sources: HashMap<String, Vec<String>> = HashMap::new();
        for cap in ts_re.captures_iter(&body) {
            let name = cap
                .get(1)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let srcs = cap
                .get(2)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            extra_sources
                .entry(name)
                .or_default()
                .extend(srcs.split_whitespace().map(|s| s.to_string()));
        }

        for cap in exe_re.captures_iter(&body) {
            let name = cap
                .get(1)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            if !is_valid_target_name(&name) {
                continue;
            }
            let inline = cap
                .get(2)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let mut all_sources: Vec<String> =
                inline.split_whitespace().map(|s| s.to_string()).collect();
            if let Some(extra) = extra_sources.get(&name) {
                all_sources.extend(extra.iter().cloned());
            }
            // Variable substitution (${VAR}) and absolute paths are not
            // resolvable without a full CMake interpreter; skip the target
            // rather than emit a misleading seed.
            if all_sources.iter().any(|s| is_pathological_source(s)) {
                continue;
            }
            let compilable: Vec<String> = all_sources
                .iter()
                .filter(|s| is_c_or_cpp_compilable(s))
                .cloned()
                .collect();
            // Header-only executables can't actually link as a binary.
            // Empty `all_sources` after a sourceless declaration with no
            // matching target_sources() is treated the same.
            if compilable.is_empty() {
                continue;
            }
            let entry =
                pick_entry(root, &dir, &compilable, &name).unwrap_or_else(|| cm.to_string());
            if !ctx.allowed(&entry) {
                continue;
            }
            let language = lang_for_path(&entry);
            let owned_files = filter_target_sources(ctx, &dir, &all_sources);
            if owned_files.is_empty() {
                continue;
            }
            let context_files = filter_target_context(ctx, cm, "CMake target declaration");
            out.push(FeatureSeed {
                title: format!("CMake binary `{name}`"),
                summary: format!("CMake `add_executable({name})` declared in {cm}"),
                kind: FeatureKind::CliCommand,
                source: "cmake-bin",
                confidence: FeatureConfidence::High,
                entry_path: entry,
                entry_symbol: Some("main".to_string()),
                entry_route: None,
                entry_command: Some(name),
                test_command: None,
                language,
                tags: vec![
                    if language == Language::Cpp {
                        "cpp"
                    } else {
                        "c"
                    }
                    .to_string(),
                    "cli".to_string(),
                ],
                owned_files,
                context_files,
                tests: Vec::new(),
                test_prefixes: vec![format!("{}tests", dir)],
            });
        }
        for cap in lib_re.captures_iter(&body) {
            let name = cap
                .get(1)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            if !is_valid_target_name(&name) {
                continue;
            }
            let inline = cap
                .get(2)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let mut all_sources: Vec<String> =
                inline.split_whitespace().map(|s| s.to_string()).collect();
            if let Some(extra) = extra_sources.get(&name) {
                all_sources.extend(extra.iter().cloned());
            }
            if all_sources.iter().any(|s| is_pathological_source(s)) {
                continue;
            }
            // Libraries can legitimately be header-only (INTERFACE), so we
            // accept zero compilable sources — but still require something
            // in the source list, otherwise the target has no files at all.
            if all_sources.is_empty() {
                continue;
            }
            let compilable: Vec<String> = all_sources
                .iter()
                .filter(|s| is_c_or_cpp_compilable(s))
                .cloned()
                .collect();
            let entry_candidates: &[String] = if compilable.is_empty() {
                &all_sources
            } else {
                &compilable
            };
            let entry =
                pick_entry(root, &dir, entry_candidates, &name).unwrap_or_else(|| cm.to_string());
            if !ctx.allowed(&entry) {
                continue;
            }
            let language = lang_for_path(&entry);
            let owned_files = filter_target_sources(ctx, &dir, &all_sources);
            // Drop the seed if every source was filtered (e.g., a vendored
            // INTERFACE library whose only file lives under vendor/).
            if owned_files.is_empty() {
                continue;
            }
            let context_files = filter_target_context(ctx, cm, "CMake target declaration");
            out.push(FeatureSeed {
                title: format!("CMake library `{name}`"),
                summary: format!("CMake `add_library({name})` declared in {cm}"),
                kind: FeatureKind::Library,
                source: "cmake-lib",
                confidence: FeatureConfidence::High,
                entry_path: entry,
                entry_symbol: None,
                entry_route: None,
                entry_command: None,
                test_command: None,
                language,
                tags: vec![
                    if language == Language::Cpp {
                        "cpp"
                    } else {
                        "c"
                    }
                    .to_string(),
                    "library".to_string(),
                ],
                owned_files,
                context_files,
                tests: Vec::new(),
                test_prefixes: vec![format!("{}tests", dir)],
            });
        }
    }
    Ok(out)
}

fn main_function_targets(
    ctx: &MapperContext,
    files: &[String],
    already_seeded: &BTreeSet<String>,
) -> Result<Vec<FeatureSeed>> {
    let root = ctx.root;
    let mut out: Vec<FeatureSeed> = Vec::new();
    let mut c_parser = Parser::new();
    let mut cpp_parser = Parser::new();
    c_parser.set_language(&tree_sitter_c::LANGUAGE.into())?;
    cpp_parser.set_language(&tree_sitter_cpp::LANGUAGE.into())?;
    for rel in files.iter().filter(|p| is_c_or_cpp_compilable(p)) {
        if already_seeded.contains(rel) {
            continue;
        }
        if !ctx.allowed(rel) {
            continue;
        }
        // Test files routinely define their own `main()` (custom harnesses,
        // googletest with --gtest_main, etc.). Without this guard the c-main
        // walker emits `C++ binary foo_test` slices that swamp legitimate
        // CLIs whenever the project's exclude_patterns don't already cover
        // tests/.
        if is_test_like_path(rel) {
            continue;
        }
        let abs = root.join(rel);
        let Ok(source) = fs::read(&abs) else { continue };
        if source.len() > 2_000_000 {
            continue; // skip huge generated sources
        }
        let parser = if lang_for_path(rel) == Language::Cpp {
            &mut cpp_parser
        } else {
            &mut c_parser
        };
        let Some(tree) = parser.parse(&source, None) else {
            continue;
        };
        if !file_defines_main(tree.root_node(), &source) {
            continue;
        }
        let language = lang_for_path(rel);
        let bin_name = rel
            .rsplit('/')
            .next()
            .and_then(|f| f.split('.').next())
            .unwrap_or(rel)
            .to_string();
        out.push(FeatureSeed {
            title: format!(
                "{} binary `{bin_name}`",
                if language == Language::Cpp {
                    "C++"
                } else {
                    "C"
                }
            ),
            summary: format!("Has a top-level `main()` at {rel}"),
            kind: FeatureKind::CliCommand,
            source: "c-main",
            confidence: FeatureConfidence::Medium,
            entry_path: rel.clone(),
            entry_symbol: Some("main".to_string()),
            entry_route: None,
            entry_command: Some(bin_name),
            test_command: None,
            language,
            tags: vec![
                if language == Language::Cpp {
                    "cpp"
                } else {
                    "c"
                }
                .to_string(),
                "cli".to_string(),
            ],
            owned_files: Vec::new(),
            context_files: Vec::new(),
            tests: Vec::new(),
            test_prefixes: Vec::new(),
        });
    }
    Ok(out)
}

fn file_defines_main(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    // Walk one level deep — `main` is always a top-level function_definition.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "function_definition" {
            continue;
        }
        let Some(declarator) = child.child_by_field_name("declarator") else {
            continue;
        };
        if function_declarator_name(declarator, source) == Some("main") {
            return true;
        }
    }
    false
}

fn function_declarator_name<'a>(node: tree_sitter::Node<'a>, source: &'a [u8]) -> Option<&'a str> {
    // Unwrap chained pointer/function declarators down to the identifier.
    let mut current = node;
    loop {
        match current.kind() {
            "function_declarator" | "pointer_declarator" | "parenthesized_declarator" => {
                current = current.child_by_field_name("declarator")?;
            }
            "identifier" | "field_identifier" => {
                return std::str::from_utf8(&source[current.byte_range()]).ok();
            }
            _ => return None,
        }
    }
}

fn collapse_backslash_continuations(s: &str) -> String {
    s.replace("\\\n", " ")
}

fn read_target_sources(body: &str, name: &str, template: &str) -> Vec<String> {
    let pattern = template.replace("{NAME}", &regex::escape(name));
    let Ok(re) = Regex::new(&pattern) else {
        return Vec::new();
    };
    let Some(cap) = re.captures(body) else {
        return Vec::new();
    };
    let raw = cap.get(1).map(|m| m.as_str()).unwrap_or_default();
    raw.split_whitespace()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn pick_entry(root: &Path, dir: &str, candidates: &[String], target_name: &str) -> Option<String> {
    // Prefer a candidate that defines main(). Fall back to first candidate
    // matching the target name or a "main.c" / "main.cpp".
    if candidates.is_empty() {
        return None;
    }
    for c in candidates {
        let full = prefix_dir(dir, c);
        if is_safe_file(root, &root.join(&full)) && full.rsplit('/').next() == Some(target_name) {
            return Some(full);
        }
    }
    let preferred: Vec<&String> = candidates
        .iter()
        .filter(|c| {
            let b = c.rsplit('/').next().unwrap_or(c);
            b.starts_with(target_name) || b.starts_with("main.")
        })
        .collect();
    if let Some(p) = preferred.into_iter().next() {
        return Some(prefix_dir(dir, p));
    }
    let first = candidates.first()?;
    Some(prefix_dir(dir, first))
}

fn prefix_dir(dir: &str, file: &str) -> String {
    if dir.is_empty() || dir == "/" {
        file.trim_start_matches('/').to_string()
    } else if file.starts_with('/') || file.starts_with("./") {
        let f = file.trim_start_matches("./");
        format!("{}{}", dir, f)
    } else {
        format!("{}{}", dir, file)
    }
}

/// Build the `owned_files` set for a build-system target while honoring
/// the configured exclude patterns: a source listed in
/// `bin_PROGRAMS_SOURCES` / `add_executable(... a.c b.c ...)` but caught
/// by `[index].exclude_patterns` must not appear in the feature record,
/// otherwise the row references a file the structural indexer ignored.
fn filter_target_sources(ctx: &MapperContext, dir: &str, sources: &[String]) -> Vec<SeedFile> {
    sources
        .iter()
        .map(|s| prefix_dir(dir, s))
        .filter(|path| ctx.allowed(path))
        .map(|path| SeedFile {
            path,
            reason: "target source".to_string(),
        })
        .collect()
}

/// Same allow-check for the single build-system manifest pointed at by
/// `context_files`. Empty result when the manifest itself is excluded —
/// rare, but it would otherwise leak a phantom file ref.
fn filter_target_context(ctx: &MapperContext, manifest: &str, reason: &str) -> Vec<SeedFile> {
    if !ctx.allowed(manifest) {
        return Vec::new();
    }
    vec![SeedFile {
        path: manifest.to_string(),
        reason: reason.to_string(),
    }]
}

fn parent_dir(rel: &str) -> String {
    match rel.rfind('/') {
        Some(i) => rel[..=i].to_string(),
        None => String::new(),
    }
}

fn is_valid_target_name(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('$')
        && !s.starts_with('\\')
        && !s.contains('(')
        && !s.contains('=')
        && !s.contains('#')
}

/// Sources that the regex layer can extract but the rest of the mapper
/// can't safely use: variable substitutions (`${APP_SOURCES}`) and
/// absolute paths (`/src/main.cpp`). Targets containing either get
/// skipped entirely — emitting them with the unsubstituted string would
/// produce phantom `owned_files` entries.
fn is_pathological_source(s: &str) -> bool {
    s.contains('$') || s.starts_with('/')
}

/// Heuristic for "this looks like a test file" used to suppress
/// `c-main` slices. Conservative on purpose: a test runner's `main()`
/// is not a CLI worth surfacing, but a binary called `mytool_test`
/// might be — we accept the rare false negative.
fn is_test_like_path(rel: &str) -> bool {
    for seg in rel.split('/') {
        if matches!(seg, "tests" | "test" | "__tests__" | "Tests") {
            return true;
        }
    }
    let base = rel.rsplit('/').next().unwrap_or(rel);
    let stem = base.split('.').next().unwrap_or(base);
    stem.ends_with("_test") || stem.ends_with("Test")
}

/// Remove CMake bracket comments (`#[[ ... ]]`, `#[=[ ... ]=]` with any
/// equals count) and `# ...` line comments. Order matters: bracket
/// comments must be detected first so the `#` opener isn't consumed by
/// the line-comment branch. Newlines inside bracket comments are
/// preserved as spaces so the line layout isn't compressed.
fn strip_cmake_comments(body: &str) -> String {
    let bytes = body.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'#' && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            let mut j = i + 2;
            let mut eq = 0;
            while j < bytes.len() && bytes[j] == b'=' {
                eq += 1;
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'[' {
                let close: Vec<u8> = std::iter::once(b']')
                    .chain(std::iter::repeat_n(b'=', eq))
                    .chain(std::iter::once(b']'))
                    .collect();
                if let Some(rel) = bytes[j + 1..]
                    .windows(close.len())
                    .position(|w| w == close.as_slice())
                {
                    // Preserve newlines inside the comment so error
                    // messages and downstream tools see consistent
                    // line numbers.
                    for &b in &bytes[i..j + 1 + rel + close.len()] {
                        if b == b'\n' {
                            out.push('\n');
                        }
                    }
                    i = j + 1 + rel + close.len();
                    continue;
                }
            }
        }
        if bytes[i] == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Dedup keyed on `(entry_path, kind)`. First seed wins, so the order
/// callers extend `seeds` in matters: autotools and CMake run before
/// main() detection so the higher-confidence build-target seed survives
/// when both fire on the same file.
fn dedup_by_entry(seeds: Vec<FeatureSeed>) -> Vec<FeatureSeed> {
    let mut seen: BTreeSet<(String, FeatureKind)> = BTreeSet::new();
    let mut out = Vec::with_capacity(seeds.len());
    for s in seeds {
        let key = (s.entry_path.clone(), s.kind);
        if seen.insert(key) {
            out.push(s);
        }
    }
    out
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
    fn detects_main_in_c_file() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "src/hello.c",
            "#include <stdio.h>\nint main(int argc, char **argv) { return 0; }\n",
        );
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let s = seeds
            .iter()
            .find(|s| s.entry_path == "src/hello.c")
            .expect("main() feature");
        assert_eq!(s.kind, FeatureKind::CliCommand);
        assert_eq!(s.entry_symbol.as_deref(), Some("main"));
        assert_eq!(s.language, Language::C);
    }

    #[test]
    fn detects_main_in_cpp_file_and_tags_as_cpp() {
        let dir = tempdir().unwrap();
        write(dir.path(), "src/app.cpp", "int main() { return 0; }\n");
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let s = seeds
            .iter()
            .find(|s| s.entry_path == "src/app.cpp")
            .expect("c++ main feature");
        assert_eq!(s.language, Language::Cpp);
        assert!(s.tags.contains(&"cpp".to_string()));
    }

    #[test]
    fn cmake_add_executable_yields_bin() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "CMakeLists.txt",
            "add_executable(myapp src/main.cpp src/util.cpp)\n",
        );
        write(dir.path(), "src/main.cpp", "int main() { return 0; }\n");
        write(dir.path(), "src/util.cpp", "");
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "cmake-bin" && s.entry_command.as_deref() == Some("myapp"))
            .expect("cmake-bin seed");
        assert_eq!(s.kind, FeatureKind::CliCommand);
    }

    #[test]
    fn cmake_add_library_yields_library() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "CMakeLists.txt",
            "add_library(corelib STATIC src/lib.c src/util.c)\n",
        );
        write(dir.path(), "src/lib.c", "int x;\n");
        write(dir.path(), "src/util.c", "int y;\n");
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "cmake-lib")
            .expect("cmake-lib seed");
        assert_eq!(s.kind, FeatureKind::Library);
        assert_eq!(s.entry_command, None);
    }

    #[test]
    fn autotools_bin_programs_extracted() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "Makefile.am",
            "bin_PROGRAMS = thing\nthing_SOURCES = thing.c util.c\n",
        );
        write(dir.path(), "thing.c", "int main(){return 0;}\n");
        write(dir.path(), "util.c", "");
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "autotools-bin")
            .expect("autotools-bin seed");
        assert_eq!(s.entry_command.as_deref(), Some("thing"));
        assert_eq!(s.kind, FeatureKind::CliCommand);
    }

    #[test]
    fn cmake_bin_with_main_function_emits_single_feature() {
        // A binary declared in CMakeLists.txt that also contains `int main()`
        // must produce exactly one cli-command seed — the build-target
        // one — not duplicates under both `cmake-bin` and `c-main`.
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "CMakeLists.txt",
            "add_executable(myapp src/main.c)\n",
        );
        write(dir.path(), "src/main.c", "int main(){return 0;}\n");
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let cli_seeds: Vec<&FeatureSeed> = seeds
            .iter()
            .filter(|s| s.kind == FeatureKind::CliCommand && s.entry_path == "src/main.c")
            .collect();
        assert_eq!(
            cli_seeds.len(),
            1,
            "expected exactly one cli-command for src/main.c, got {:#?}",
            cli_seeds
        );
        assert_eq!(
            cli_seeds[0].source, "cmake-bin",
            "build-target seed must win over c-main"
        );
    }

    #[test]
    fn autotools_bin_with_main_function_emits_single_feature() {
        // Same dedup contract as the CMake test, anchored on Makefile.am.
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "Makefile.am",
            "bin_PROGRAMS = thing\nthing_SOURCES = thing.c\n",
        );
        write(dir.path(), "thing.c", "int main(){return 0;}\n");
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let cli_seeds: Vec<&FeatureSeed> = seeds
            .iter()
            .filter(|s| s.kind == FeatureKind::CliCommand && s.entry_path == "thing.c")
            .collect();
        assert_eq!(
            cli_seeds.len(),
            1,
            "expected exactly one cli-command for thing.c, got {:#?}",
            cli_seeds
        );
        assert_eq!(cli_seeds[0].source, "autotools-bin");
    }

    // ---------- regressions ported from clawpatch PR #26 ----------

    #[test]
    fn c_main_skips_files_under_tests_directory() {
        // googletest / catch2 / custom harness test files routinely define
        // their own `main()`; they must not surface as CLI features.
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "tests/myapp_test.cpp",
            "int main() { return 0; }\n",
        );
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        assert!(
            !seeds.iter().any(|s| s.source == "c-main"),
            "tests/* main() must not become a CLI feature, got: {:#?}",
            seeds
        );
    }

    #[test]
    fn c_main_skips_underscore_test_suffix() {
        let dir = tempdir().unwrap();
        write(dir.path(), "src/foo_test.c", "int main() { return 0; }\n");
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        assert!(!seeds.iter().any(|s| s.source == "c-main"));
    }

    #[test]
    fn cmake_uppercase_keywords_match() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "CMakeLists.txt",
            "ADD_EXECUTABLE(upper src/upper.c)\nADD_LIBRARY(upperlib STATIC src/upperlib.c)\n",
        );
        write(dir.path(), "src/upper.c", "int main(){return 0;}\n");
        write(dir.path(), "src/upperlib.c", "int x;\n");
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        assert!(
            seeds
                .iter()
                .any(|s| s.source == "cmake-bin" && s.entry_command.as_deref() == Some("upper")),
            "uppercase ADD_EXECUTABLE not matched: {:#?}",
            seeds.iter().map(|s| &s.title).collect::<Vec<_>>()
        );
        assert!(seeds.iter().any(|s| s.source == "cmake-lib"));
    }

    #[test]
    fn cmake_numeric_prefix_and_dotted_target_names() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "CMakeLists.txt",
            "add_executable(7zip src/seven.c)\nadd_library(foo.bar STATIC src/dot.c)\n",
        );
        write(dir.path(), "src/seven.c", "int main(){return 0;}\n");
        write(dir.path(), "src/dot.c", "int dot(){return 0;}\n");
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        assert!(
            seeds
                .iter()
                .any(|s| s.entry_command.as_deref() == Some("7zip")),
            "numeric-prefix target name rejected"
        );
        assert!(
            seeds.iter().any(|s| s.title == "CMake library `foo.bar`"),
            "dotted target name rejected"
        );
    }

    #[test]
    fn cmake_late_bound_target_sources_merge() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "CMakeLists.txt",
            "add_executable(latebin)\n\
             target_sources(latebin PRIVATE src/late_main.c src/late_util.c)\n",
        );
        write(dir.path(), "src/late_main.c", "int main(){return 0;}\n");
        write(dir.path(), "src/late_util.c", "int util(){return 0;}\n");
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let s = seeds
            .iter()
            .find(|s| s.entry_command.as_deref() == Some("latebin"))
            .expect("latebin seed");
        let owned_paths: Vec<&str> = s.owned_files.iter().map(|f| f.path.as_str()).collect();
        assert!(owned_paths.contains(&"src/late_main.c"));
        assert!(owned_paths.contains(&"src/late_util.c"));
    }

    #[test]
    fn cmake_bracket_comments_strip_commented_targets() {
        // Commented-out CMake targets must not surface as cmake-bin
        // features. The underlying source file is still on disk so
        // c-main may surface it separately — that's expected and not
        // what this regression covers.
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "CMakeLists.txt",
            "#[[\nadd_executable(commented src/commented.c)\n]]\n\
             add_executable(real src/real.c)\n",
        );
        write(dir.path(), "src/commented.c", "int main(){return 0;}\n");
        write(dir.path(), "src/real.c", "int main(){return 0;}\n");
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        assert!(
            !seeds
                .iter()
                .any(|s| s.source == "cmake-bin"
                    && s.entry_command.as_deref() == Some("commented")),
            "commented-out CMake target leaked through bracket-comment stripping"
        );
        assert!(
            seeds
                .iter()
                .any(|s| s.source == "cmake-bin" && s.entry_command.as_deref() == Some("real")),
            "real CMake target dropped"
        );
    }

    #[test]
    fn cmake_skips_variable_substituted_sources() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "CMakeLists.txt",
            "add_executable(varapp ${APP_SOURCES})\n",
        );
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        assert!(
            !seeds
                .iter()
                .any(|s| s.entry_command.as_deref() == Some("varapp")),
            "variable-substituted source list must not yield a target"
        );
    }

    #[test]
    fn cmake_skips_absolute_path_sources() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "CMakeLists.txt",
            "add_executable(absout /src/main.cpp)\n",
        );
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        assert!(
            !seeds
                .iter()
                .any(|s| s.entry_command.as_deref() == Some("absout")),
            "absolute path source must not yield a target"
        );
    }

    #[test]
    fn cmake_skips_header_only_executable() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "CMakeLists.txt",
            "add_executable(headerapp include/headers.hpp)\n",
        );
        write(dir.path(), "include/headers.hpp", "void f();\n");
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        assert!(
            !seeds
                .iter()
                .any(|s| s.entry_command.as_deref() == Some("headerapp")),
            "executable with only header sources is impossible"
        );
    }

    #[test]
    fn cmake_interface_library_with_headers_emits() {
        // INTERFACE libraries can legitimately have only header sources;
        // they should still surface so reviewers can see the API surface.
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "CMakeLists.txt",
            "add_library(headers INTERFACE include/headers.hpp)\n",
        );
        write(dir.path(), "include/headers.hpp", "void f();\n");
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        assert!(
            seeds.iter().any(|s| s.title == "CMake library `headers`"),
            "INTERFACE library with headers dropped: {:#?}",
            seeds.iter().map(|s| &s.title).collect::<Vec<_>>()
        );
    }

    #[test]
    fn cmake_skips_vendored_interface_when_excluded() {
        // A vendored INTERFACE library whose only file lives under
        // vendor/ should drop because the structural indexer won't
        // index that file — emitting it would leak a phantom owned_file.
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "CMakeLists.txt",
            "add_library(vendored INTERFACE vendor/dep.hpp)\n",
        );
        write(dir.path(), "vendor/dep.hpp", "void f();\n");
        let mut builder = globset::GlobSetBuilder::new();
        builder.add(globset::Glob::new("**/vendor/**").unwrap());
        let excludes = builder.build().unwrap();
        let ctx = MapperContext {
            root: dir.path(),
            excludes: Some(&excludes),
        };
        let seeds = CCppMapper.map(&ctx).unwrap();
        assert!(
            !seeds.iter().any(|s| s.title == "CMake library `vendored`"),
            "vendored INTERFACE library should drop when its only file is excluded"
        );
    }
}
