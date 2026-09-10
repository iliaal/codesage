//! C / C++ mapper: detects `main()` functions via tree-sitter, plus
//! `bin_PROGRAMS` + `lib_LTLIBRARIES` from autotools `Makefile.am` and
//! `add_executable` + `add_library` from CMake.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::Path;

use anyhow::Result;
use codesage_protocol::{FeatureConfidence, FeatureKind, Language};
use regex::Regex;
use tree_sitter::Parser;

use crate::mappers::shared::{is_safe_file, read_to_string_bounded, walk_files};
use crate::mappers::types::{FeatureMapper, FeatureSeed, MapperContext, SeedFile};
use crate::nearby_tests::is_c_or_cpp_test_path;

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
        // Build-target seeds take precedence over generic main() detection.
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
        || is_cuda_source(rel)
}

fn is_c_or_cpp_compilable(rel: &str) -> bool {
    rel.ends_with(".c")
        || rel.ends_with(".cpp")
        || rel.ends_with(".cc")
        || rel.ends_with(".cxx")
        || rel.ends_with(".cu")
}

/// CUDA source / header: `.cu` is a compilable translation unit, `.cuh` is
/// a CUDA header. Both classify as C/C++ for mapping purposes (there is no
/// distinct `Cuda` language); the `cuda` tag on the seed is what marks them.
fn is_cuda_source(rel: &str) -> bool {
    rel.ends_with(".cu") || rel.ends_with(".cuh")
}

/// Classify a CMake `add_executable(...)` target as a test suite when the
/// target name ends in `tests?` (with optional `_` or `-` separator,
/// case-insensitive), or — when the name is neutral — every compilable
/// source matches the test path heuristic.
///
/// Target-name match wins outright. Source-path match alone (with a
/// neutral target name) only flips the classification when *all*
/// compilable sources are test-shaped: a single helper file named
/// `test_mode.c` inside a regular binary's source list should not turn
/// `app` into a test suite.
fn is_cmake_test_executable(name: &str, sources: &[String]) -> bool {
    let lower = name.to_ascii_lowercase();
    let ends_in_tests = lower == "test"
        || lower == "tests"
        || lower.ends_with("_test")
        || lower.ends_with("-test")
        || lower.ends_with("_tests")
        || lower.ends_with("-tests");
    if ends_in_tests {
        return true;
    }
    let compilable: Vec<&String> = sources
        .iter()
        .filter(|s| is_c_or_cpp_compilable(s))
        .collect();
    if compilable.is_empty() {
        return false;
    }
    compilable.iter().all(|s| is_c_or_cpp_test_path(s))
}

/// Return the last valid project name in the file. This approximation ignores
/// CMake scopes and declaration order relative to each target.
fn cmake_project_name(body: &str) -> Option<String> {
    let mut name = None;
    for args in cmake_command_args(body, "project") {
        let words = cmake_split_args(&args);
        if let Some(first) = words.first()
            && is_valid_target_name(first)
        {
            name = Some(first.clone());
        }
    }
    name
}

/// Resolve a CMake target name that may reference `${PROJECT_NAME}` or
/// `${CMAKE_PROJECT_NAME}` against the most recent `project()` call.
/// Returns the resolved name when the substitution is unambiguous;
/// returns the input unchanged otherwise so the caller can apply the
/// existing skip-unresolved-variable logic.
fn resolve_cmake_target_name(raw: &str, project_name: Option<&str>) -> String {
    if let Some(pn) = project_name
        && (raw == "${PROJECT_NAME}" || raw == "${CMAKE_PROJECT_NAME}")
    {
        return pn.to_string();
    }
    raw.to_string()
}

/// Expand Automake source-directory variables in a path.
///
/// Paths remain relative to the makefile directory, which filter_target_sources
/// prefixes later. Root-relative top_srcdir/top_builddir and unknown variables
/// remain unresolved; stripping them would resolve against the wrong directory.
fn expand_automake_vars(source: &str, _makefile_dir: &str) -> String {
    let mut result = source.to_string();
    for var in ["$(srcdir)/", "${srcdir}/"] {
        result = result.replace(var, "");
    }
    result
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
        || is_cuda_source(rel)
    {
        Language::Cpp
    } else {
        Language::C
    }
}

/// Build the tag vector for a CMake target seed: the language tag
/// (`c`/`cpp`), a kind tag (`cli`/`library`/`test`), and `cuda` when the
/// target was declared via a `cuda_add_*` macro or pulls in any `.cu` /
/// `.cuh` source.
fn cmake_target_tags(language: Language, kind_tag: &str, is_cuda: bool) -> Vec<String> {
    let mut tags = vec![
        if language == Language::Cpp {
            "cpp"
        } else {
            "c"
        }
        .to_string(),
        kind_tag.to_string(),
    ];
    if is_cuda {
        tags.push("cuda".to_string());
    }
    tags
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
        let body = read_to_string_bounded(&path)
            .ok()
            .flatten()
            .unwrap_or_default();
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
                let sources: Vec<String> = sources
                    .iter()
                    .map(|s| expand_automake_vars(s, &dir))
                    .collect();
                let entry_candidates = sources
                    .iter()
                    .filter(|s| is_c_or_cpp_compilable(s))
                    .cloned()
                    .collect::<Vec<_>>();
                let entry = pick_entry(root, &dir, &entry_candidates, name)
                    .unwrap_or_else(|| mf.to_string());
                if !ctx.allowed(&entry) {
                    continue;
                }
                let language = lang_for_path(&entry);
                let owned_files = filter_target_sources(ctx, &dir, &sources);
                let context_files = filter_target_context(ctx, mf, "build target declaration");
                out.push(FeatureSeed {
                    summary: format!("Makefile.am `bin_PROGRAMS` target declared in {mf}"),
                    source: "autotools-bin",
                    confidence: FeatureConfidence::High,
                    entry_symbol: Some("main".to_string()),
                    entry_command: Some(name.to_string()),
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
                    test_prefixes: vec![format!("{}tests", dir)],
                    ..FeatureSeed::new(
                        FeatureKind::CliCommand,
                        language,
                        format!("Autotools binary `{name}`"),
                        entry,
                    )
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
                let sources: Vec<String> = sources
                    .iter()
                    .map(|s| expand_automake_vars(s, &dir))
                    .collect();
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
                    summary: format!("Makefile.am `lib_LTLIBRARIES` target declared in {mf}"),
                    source: "autotools-lib",
                    confidence: FeatureConfidence::High,
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
                    test_prefixes: vec![format!("{}tests", dir)],
                    ..FeatureSeed::new(
                        FeatureKind::Library,
                        language,
                        format!("Autotools library `{name}`"),
                        entry,
                    )
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
    // String-aware walking ignores quoted command text and parentheses in paths.
    for cm in cmake_files {
        let path = root.join(cm);
        let raw = read_to_string_bounded(&path)
            .ok()
            .flatten()
            .unwrap_or_default();
        let body = strip_cmake_comments(&raw);
        let dir = parent_dir(cm);
        let project_name = cmake_project_name(&body);

        // Collect target_sources first: additions may follow target declarations.
        let mut extra_sources: HashMap<String, Vec<String>> = HashMap::new();
        for args in cmake_command_args(&body, "target_sources") {
            let mut words = cmake_split_args(&args);
            if words.is_empty() {
                continue;
            }
            let name = resolve_cmake_target_name(&words.remove(0), project_name.as_deref());
            // Tolerate missing scope keywords; present keywords are not sources.
            strip_target_sources_scope(&mut words);
            extra_sources.entry(name).or_default().extend(words);
        }

        // FindCUDA macros imply CUDA even without a .cu/.cuh source.
        let exe_calls: Vec<(String, bool)> = cmake_command_args(&body, "add_executable")
            .into_iter()
            .map(|a| (a, false))
            .chain(
                cmake_command_args(&body, "cuda_add_executable")
                    .into_iter()
                    .map(|a| (a, true)),
            )
            .collect();
        for (args, macro_is_cuda) in exe_calls {
            let mut words = cmake_split_args(&args);
            if words.is_empty() {
                continue;
            }
            let name = resolve_cmake_target_name(&words.remove(0), project_name.as_deref());
            if !is_valid_target_name(&name) {
                continue;
            }
            strip_cmake_target_options(&mut words);
            let mut all_sources: Vec<String> = words;
            if let Some(extra) = extra_sources.get(&name) {
                all_sources.extend(extra.iter().cloned());
            }
            let is_cuda = macro_is_cuda || all_sources.iter().any(|s| is_cuda_source(s));
            // Unresolved variables or absolute paths make target ownership uncertain.
            if all_sources.iter().any(|s| is_pathological_source(s)) {
                continue;
            }
            let compilable: Vec<String> = all_sources
                .iter()
                .filter(|s| is_c_or_cpp_compilable(s))
                .cloned()
                .collect();
            // Executables need a compilable source; headers alone cannot link.
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
            if is_cmake_test_executable(&name, &all_sources) {
                let test_paths: Vec<&SeedFile> = owned_files
                    .iter()
                    .filter(|f| is_c_or_cpp_test_path(&f.path))
                    .collect();
                let tests: Vec<crate::mappers::types::SeedTest> = test_paths
                    .iter()
                    .map(|f| crate::mappers::types::SeedTest {
                        path: f.path.clone(),
                        command: None,
                    })
                    .collect();
                out.push(FeatureSeed {
                    summary: format!("CMake test executable `{name}` declared in {cm}"),
                    source: "cmake-test",
                    confidence: FeatureConfidence::High,
                    tags: cmake_target_tags(language, "test", is_cuda),
                    owned_files,
                    context_files,
                    tests,
                    test_prefixes: vec![format!("{}tests", dir)],
                    ..FeatureSeed::new(
                        FeatureKind::TestSuite,
                        language,
                        format!("CMake test suite `{name}`"),
                        entry,
                    )
                });
                continue;
            }
            out.push(FeatureSeed {
                summary: format!("CMake `add_executable({name})` declared in {cm}"),
                source: "cmake-bin",
                confidence: FeatureConfidence::High,
                entry_symbol: Some("main".to_string()),
                entry_command: Some(name.clone()),
                tags: cmake_target_tags(language, "cli", is_cuda),
                owned_files,
                context_files,
                test_prefixes: vec![format!("{}tests", dir)],
                ..FeatureSeed::new(
                    FeatureKind::CliCommand,
                    language,
                    format!("CMake binary `{name}`"),
                    entry,
                )
            });
        }
        let lib_calls: Vec<(String, bool)> = cmake_command_args(&body, "add_library")
            .into_iter()
            .map(|a| (a, false))
            .chain(
                cmake_command_args(&body, "cuda_add_library")
                    .into_iter()
                    .map(|a| (a, true)),
            )
            .collect();
        for (args, macro_is_cuda) in lib_calls {
            let mut words = cmake_split_args(&args);
            if words.is_empty() {
                continue;
            }
            let name = resolve_cmake_target_name(&words.remove(0), project_name.as_deref());
            if !is_valid_target_name(&name) {
                continue;
            }
            strip_library_type_keyword(&mut words);
            strip_cmake_target_options(&mut words);
            let mut all_sources: Vec<String> = words;
            if let Some(extra) = extra_sources.get(&name) {
                all_sources.extend(extra.iter().cloned());
            }
            let is_cuda = macro_is_cuda || all_sources.iter().any(|s| is_cuda_source(s));
            if all_sources.iter().any(|s| is_pathological_source(s)) {
                continue;
            }
            // Header-only libraries are valid; targets without any files are not.
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
            if owned_files.is_empty() {
                continue;
            }
            let context_files = filter_target_context(ctx, cm, "CMake target declaration");
            out.push(FeatureSeed {
                summary: format!("CMake `add_library({name})` declared in {cm}"),
                source: "cmake-lib",
                confidence: FeatureConfidence::High,
                tags: cmake_target_tags(language, "library", is_cuda),
                owned_files,
                context_files,
                test_prefixes: vec![format!("{}tests", dir)],
                ..FeatureSeed::new(
                    FeatureKind::Library,
                    language,
                    format!("CMake library `{name}`"),
                    entry,
                )
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
        // A test harness's main() is not a CLI feature.
        if is_c_or_cpp_test_path(rel) {
            continue;
        }
        let abs = root.join(rel);
        // Gate allocations before reading; raw bytes allow non-UTF-8 C comments.
        if !fs::metadata(&abs).is_ok_and(|m| m.len() <= 2_000_000) {
            continue;
        }
        let Ok(source) = fs::read(&abs) else { continue };
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
            summary: format!("Has a top-level `main()` at {rel}"),
            source: "c-main",
            entry_symbol: Some("main".to_string()),
            entry_command: Some(bin_name.clone()),
            tags: cmake_target_tags(language, "cli", is_cuda_source(rel)),
            ..FeatureSeed::new(
                FeatureKind::CliCommand,
                language,
                format!(
                    "{} binary `{bin_name}`",
                    if language == Language::Cpp {
                        "C++"
                    } else {
                        "C"
                    }
                ),
                rel.clone(),
            )
        });
    }
    Ok(out)
}

fn file_defines_main(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    // Only direct top-level definitions qualify.
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
    // Rank by filename, without parsing candidate bodies.
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

/// Unresolved variables and absolute paths cannot produce reliable owned-file refs.
fn is_pathological_source(s: &str) -> bool {
    s.contains('$') || s.starts_with('/')
}

/// Remove bracket and line comments while preserving newlines and string contents.
/// Bracket comments must be checked before line comments consume their `#` opener.
/// ASCII delimiters cannot match UTF-8 continuation bytes; copy other text as scalars.
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
        // Both indices are character boundaries, including for bracket arguments.
        if let Some(j) = cmake_skip_string_like(bytes, i) {
            out.push_str(&body[i..j]);
            i = j;
            continue;
        }
        if bytes[i] == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        let ch = body[i..].chars().next().expect("byte index inside body");
        let width = ch.len_utf8();
        out.push(ch);
        i += width;
    }
    out
}

/// Walks `body` looking for top-level invocations of `command(...)`. For
/// each occurrence, returns the unparsed argument slice between the
/// outer parens. Skips text inside quoted strings and bracket arguments
/// (`[[...]]`, `[=[...]=]`) so command-like text inside string literals
/// doesn't produce spurious matches. Handles arbitrarily nested parens
/// and quoted args that contain `)`. Command match is case-insensitive
/// and bounded so `add_executables` does not match `add_executable`.
fn cmake_command_args(body: &str, command: &str) -> Vec<String> {
    let bytes = body.as_bytes();
    let cmd = command.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(j) = cmake_skip_string_like(bytes, i) {
            i = j;
            continue;
        }
        if i + cmd.len() <= bytes.len()
            && bytes[i..i + cmd.len()].eq_ignore_ascii_case(cmd)
            && !is_cmake_identifier_byte(if i == 0 { None } else { Some(bytes[i - 1]) })
            && !is_cmake_identifier_byte(bytes.get(i + cmd.len()).copied())
        {
            let mut open = i + cmd.len();
            while open < bytes.len() && bytes[open].is_ascii_whitespace() {
                open += 1;
            }
            if open < bytes.len()
                && bytes[open] == b'('
                && let Some(close) = cmake_find_close_paren(bytes, open)
            {
                // ASCII delimiters guarantee character boundaries.
                out.push(body[open + 1..close].to_string());
                i = close + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// If the byte at `i` opens a string-like construct (quoted `"..."` or
/// bracket `[[...]]` / `[=[...]=]` argument), returns the byte index
/// just past its close; otherwise `None`. Unterminated openers consume
/// the rest of the body so the outer walker doesn't get stuck.
fn cmake_skip_string_like(bytes: &[u8], i: usize) -> Option<usize> {
    if i >= bytes.len() {
        return None;
    }
    if bytes[i] == b'"' {
        return Some(cmake_quoted_end(bytes, i));
    }
    cmake_bracket_end(bytes, i)
}

/// `bytes[start] == b'"'`. Returns the index just past the matching
/// closing `"`. Inside the quote, `\X` escapes the next byte (so `\"`
/// does not terminate). Falls through to `bytes.len()` on an
/// unterminated quote.
fn cmake_quoted_end(bytes: &[u8], start: usize) -> usize {
    let mut j = start + 1;
    while j < bytes.len() {
        if bytes[j] == b'\\' && j + 1 < bytes.len() {
            j += 2;
            continue;
        }
        if bytes[j] == b'"' {
            return j + 1;
        }
        j += 1;
    }
    bytes.len()
}

/// Recognizes a CMake bracket-argument opener `[=*[` at `start`. Returns
/// the index just past the matching `]=*]` closer (with the same number
/// of equals), or `bytes.len()` if unterminated. Returns `None` when
/// `start` does not point at a valid opener.
fn cmake_bracket_end(bytes: &[u8], start: usize) -> Option<usize> {
    if bytes.get(start) != Some(&b'[') {
        return None;
    }
    let mut k = start + 1;
    while bytes.get(k) == Some(&b'=') {
        k += 1;
    }
    if bytes.get(k) != Some(&b'[') {
        return None;
    }
    let eq = k - start - 1;
    let body_start = k + 1;
    let closer_len = eq + 2;
    if body_start >= bytes.len() {
        return Some(bytes.len());
    }
    let mut j = body_start;
    while j + closer_len <= bytes.len() {
        if bytes[j] == b']'
            && bytes[j + closer_len - 1] == b']'
            && bytes[j + 1..j + 1 + eq].iter().all(|&b| b == b'=')
        {
            return Some(j + closer_len);
        }
        j += 1;
    }
    Some(bytes.len())
}

/// `bytes[open] == b'('`. Walks forward keeping a paren-depth counter
/// (skipping string-like spans) and returns the index of the matching
/// `)`, or `None` if no balancing `)` is found.
fn cmake_find_close_paren(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth: usize = 1;
    let mut i = open + 1;
    while i < bytes.len() {
        if let Some(j) = cmake_skip_string_like(bytes, i) {
            i = j;
            continue;
        }
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn is_cmake_identifier_byte(b: Option<u8>) -> bool {
    matches!(b, Some(b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_'))
}

/// Splits a CMake command-args slice into individual words. Quoted
/// strings (`"..."`) and bracket arguments (`[[...]]`) survive whitespace
/// as single tokens — important for source paths that contain spaces.
/// Unquoted words are then split on `;` (CMake list separator); quoted
/// values are NOT split that way (per CMake's documented semantics).
/// Returns each word's unescaped content.
fn cmake_split_args(args: &str) -> Vec<String> {
    let bytes = args.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if bytes[i] == b'"' {
            let end = cmake_quoted_end(bytes, i);
            let inner_end = end.saturating_sub(1).max(i + 1);
            let value = unescape_cmake_quoted(&args[i + 1..inner_end]);
            if !value.is_empty() {
                out.push(value);
            }
            i = end;
            continue;
        }
        if let Some(end) = cmake_bracket_end(bytes, i) {
            // Bracket arguments have equal-length delimiters and no escape processing.
            let mut k = i + 1;
            while bytes.get(k) == Some(&b'=') {
                k += 1;
            }
            let opener_len = (k + 1) - i;
            let closer_len = opener_len;
            let content_end = end.saturating_sub(closer_len).max(i + opener_len);
            let inner = &args[i + opener_len..content_end];
            if !inner.is_empty() {
                out.push(inner.to_string());
            }
            i = end;
            continue;
        }
        let start = i;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'"'
            && bytes[i] != b'['
        {
            i += 1;
        }
        let word = &args[start..i];
        for seg in word.split(';').filter(|s| !s.is_empty()) {
            out.push(seg.to_string());
        }
    }
    out
}

/// Simplified path unescaping: `\X` becomes `X`, including `\n` becoming `n`.
fn unescape_cmake_quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn strip_library_type_keyword(words: &mut Vec<String>) {
    if let Some(first) = words.first() {
        let kw = first.to_ascii_uppercase();
        if matches!(
            kw.as_str(),
            "SHARED" | "STATIC" | "MODULE" | "OBJECT" | "INTERFACE"
        ) {
            words.remove(0);
        }
    }
}

fn strip_target_sources_scope(words: &mut Vec<String>) {
    words.retain(|word| {
        let kw = word.to_ascii_uppercase();
        !matches!(kw.as_str(), "PRIVATE" | "PUBLIC" | "INTERFACE")
    });
}

/// Prevent executable/library option keywords from becoming owned-file paths.
fn strip_cmake_target_options(words: &mut Vec<String>) {
    words.retain(|word| {
        let kw = word.to_ascii_uppercase();
        !matches!(kw.as_str(), "WIN32" | "MACOSX_BUNDLE" | "EXCLUDE_FROM_ALL")
    });
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

    #[test]
    fn c_main_skips_files_under_tests_directory() {
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
             target_sources(latebin PRIVATE src/late_main.c src/late_util.c PUBLIC include/late.h INTERFACE include/api.h)\n",
        );
        write(dir.path(), "src/late_main.c", "int main(){return 0;}\n");
        write(dir.path(), "src/late_util.c", "int util(){return 0;}\n");
        write(dir.path(), "include/late.h", "int late(void);\n");
        write(dir.path(), "include/api.h", "int api(void);\n");
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
        assert!(owned_paths.contains(&"include/late.h"));
        assert!(owned_paths.contains(&"include/api.h"));
        assert!(!owned_paths.contains(&"PUBLIC"));
        assert!(!owned_paths.contains(&"INTERFACE"));
    }

    #[test]
    fn cmake_bracket_comments_strip_commented_targets() {
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

    #[test]
    fn cmake_test_target_emits_test_suite_not_cli() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "CMakeLists.txt",
            "add_executable(foo_tests test_main.cpp helper_test.cpp)\n",
        );
        write(dir.path(), "test_main.cpp", "int main(){return 0;}\n");
        write(dir.path(), "helper_test.cpp", "void noop(){}\n");
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let test_seed = seeds
            .iter()
            .find(|s| s.source == "cmake-test")
            .expect("cmake-test seed missing");
        assert_eq!(test_seed.kind, FeatureKind::TestSuite);
        assert!(test_seed.tags.iter().any(|t| t == "test"));
        assert!(
            test_seed.entry_command.is_none(),
            "test suite has no `command`"
        );
        assert!(
            !seeds.iter().any(|s| s.source == "cmake-bin" && s.entry_command.as_deref() == Some("foo_tests")),
            "test-named target should not also emit a cli-command seed"
        );
    }

    #[test]
    fn cmake_test_target_by_source_paths_only() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "CMakeLists.txt",
            "add_executable(runner tests/main.c)\n",
        );
        write(dir.path(), "tests/main.c", "int main(){return 0;}\n");
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        assert!(
            seeds
                .iter()
                .any(|s| s.source == "cmake-test" && s.kind == FeatureKind::TestSuite),
            "neutral name with test-path sources should classify as test-suite; got: {:?}",
            seeds.iter().map(|s| s.source).collect::<Vec<_>>()
        );
    }

    #[test]
    fn cmake_non_test_target_remains_cli_command() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "CMakeLists.txt",
            "add_executable(server src/main.cpp)\n",
        );
        write(dir.path(), "src/main.cpp", "int main(){return 0;}\n");
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        assert!(
            seeds
                .iter()
                .any(|s| s.source == "cmake-bin" && s.kind == FeatureKind::CliCommand),
            "non-test target should remain cmake-bin"
        );
        assert!(
            !seeds.iter().any(|s| s.source == "cmake-test"),
            "non-test target must not emit a cmake-test seed"
        );
    }

    #[test]
    fn c_main_in_test_prefixed_file_is_suppressed() {
        let dir = tempdir().unwrap();
        write(dir.path(), "src/tool.c", "int main(void) { return 0; }\n");
        write(
            dir.path(),
            "src/test_harness.c",
            "int main(void) { return 0; }\n",
        );
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        assert!(
            seeds
                .iter()
                .any(|s| s.source == "c-main" && s.entry_path == "src/tool.c"),
            "regular main() file must still seed: {seeds:?}"
        );
        assert!(
            !seeds.iter().any(|s| s.entry_path == "src/test_harness.c"),
            "test_-prefixed main() file must be suppressed: {seeds:?}"
        );
    }

    #[test]
    fn strip_cmake_comments_preserves_non_ascii_paths() {
        let stripped = super::strip_cmake_comments("add_executable(app src/café.c)\n");
        assert!(
            stripped.contains("café"),
            "non-ASCII path lost UTF-8 round-trip: {stripped:?}"
        );
    }

    #[test]
    fn cmake_ignores_command_text_inside_strings() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "CMakeLists.txt",
            "message(\"add_executable(fake src/main.c)\")\n\
             message([[add_library(fake_lib src/lib.c)]])\n\
             add_executable(real src/real.c)\n",
        );
        write(dir.path(), "src/main.c", "int main(){return 0;}\n");
        write(dir.path(), "src/lib.c", "int lib(){return 0;}\n");
        write(dir.path(), "src/real.c", "int main(){return 0;}\n");
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        assert!(
            !seeds
                .iter()
                .any(|s| s.entry_command.as_deref() == Some("fake")),
            "string-embedded add_executable leaked as a feature"
        );
        assert!(
            !seeds.iter().any(|s| s.title == "CMake library `fake_lib`"),
            "bracket-embedded add_library leaked as a feature"
        );
        assert!(
            seeds
                .iter()
                .any(|s| s.source == "cmake-bin" && s.entry_command.as_deref() == Some("real")),
            "real add_executable was dropped"
        );
    }

    #[test]
    fn cmake_quoted_source_paths_with_spaces() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "CMakeLists.txt",
            "add_executable(app \"src/main file.cpp\" \"src/helper file.cpp\")\n",
        );
        write(dir.path(), "src/main file.cpp", "int main(){return 0;}\n");
        write(dir.path(), "src/helper file.cpp", "int help(){return 0;}\n");
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "cmake-bin" && s.entry_command.as_deref() == Some("app"))
            .expect("cmake-bin seed for app");
        let owned: Vec<&str> = s.owned_files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            owned.contains(&"src/main file.cpp"),
            "quoted space-bearing source dropped: {owned:?}"
        );
        assert!(
            owned.contains(&"src/helper file.cpp"),
            "second quoted space-bearing source dropped: {owned:?}"
        );
    }

    #[test]
    fn cmake_quoted_source_paths_with_paren() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "CMakeLists.txt",
            "add_executable(legacy \"src/foo(v1).cpp\" \"src/main.cpp\")\n",
        );
        write(dir.path(), "src/foo(v1).cpp", "int helper(){return 0;}\n");
        write(dir.path(), "src/main.cpp", "int main(){return 0;}\n");
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "cmake-bin" && s.entry_command.as_deref() == Some("legacy"))
            .expect("cmake-bin seed for legacy");
        let owned: Vec<&str> = s.owned_files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            owned.contains(&"src/foo(v1).cpp"),
            "quoted source with embedded `)` was lost: {owned:?}"
        );
        assert!(
            owned.contains(&"src/main.cpp"),
            "trailing source after `)`-bearing quoted path was lost: {owned:?}"
        );
    }

    #[test]
    fn cmake_add_library_strips_type_keyword() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "CMakeLists.txt",
            "add_library(mylib SHARED src/a.c src/b.c)\n",
        );
        write(dir.path(), "src/a.c", "int a(){return 0;}\n");
        write(dir.path(), "src/b.c", "int b(){return 0;}\n");
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "cmake-lib" && s.title == "CMake library `mylib`")
            .expect("cmake-lib seed");
        let owned: Vec<&str> = s.owned_files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            !owned.iter().any(|p| p.ends_with("SHARED")),
            "library type keyword leaked as a source: {owned:?}"
        );
        assert!(
            owned.contains(&"src/a.c") && owned.contains(&"src/b.c"),
            "real sources missing: {owned:?}"
        );
    }

    #[test]
    fn cmake_strips_executable_and_library_option_keywords() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "CMakeLists.txt",
            "add_executable(app WIN32 MACOSX_BUNDLE main.c)\n\
             add_library(foo STATIC EXCLUDE_FROM_ALL a.c)\n",
        );
        write(dir.path(), "main.c", "int main(){return 0;}\n");
        write(dir.path(), "a.c", "int a(){return 0;}\n");
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();

        let exe = seeds
            .iter()
            .find(|s| s.source == "cmake-bin" && s.entry_command.as_deref() == Some("app"))
            .expect("cmake-bin seed for `app`");
        let exe_owned: Vec<&str> = exe.owned_files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            !exe_owned
                .iter()
                .any(|p| p.ends_with("WIN32") || p.ends_with("MACOSX_BUNDLE")),
            "executable option keyword leaked as a source: {exe_owned:?}"
        );
        assert!(
            exe_owned.contains(&"main.c"),
            "real source missing: {exe_owned:?}"
        );

        let lib = seeds
            .iter()
            .find(|s| s.source == "cmake-lib" && s.title == "CMake library `foo`")
            .expect("cmake-lib seed for `foo`");
        let lib_owned: Vec<&str> = lib.owned_files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            !lib_owned.iter().any(|p| p.ends_with("EXCLUDE_FROM_ALL")),
            "EXCLUDE_FROM_ALL leaked as a source: {lib_owned:?}"
        );
        assert!(
            lib_owned.contains(&"a.c"),
            "real source missing: {lib_owned:?}"
        );
    }

    #[test]
    fn cmake_cuda_add_executable_tagged_cuda() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "CMakeLists.txt",
            "cuda_add_executable(sim src/sim.cu)\n",
        );
        write(dir.path(), "src/sim.cu", "int main(){return 0;}\n");
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "cmake-bin" && s.entry_command.as_deref() == Some("sim"))
            .expect("cuda_add_executable seed missing");
        assert!(s.tags.iter().any(|t| t == "cuda"), "tags: {:?}", s.tags);
    }

    #[test]
    fn cmake_add_executable_with_cu_source_tagged_cuda() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "CMakeLists.txt",
            "add_executable(app src/main.cpp src/kernel.cu)\n",
        );
        write(dir.path(), "src/main.cpp", "int main(){return 0;}\n");
        write(dir.path(), "src/kernel.cu", "__global__ void k(){}\n");
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "cmake-bin" && s.entry_command.as_deref() == Some("app"))
            .expect("add_executable seed missing");
        assert!(s.tags.iter().any(|t| t == "cuda"), "tags: {:?}", s.tags);
        let owned: Vec<&str> = s.owned_files.iter().map(|f| f.path.as_str()).collect();
        assert!(owned.contains(&"src/kernel.cu"), "owned: {owned:?}");
    }

    #[test]
    fn standalone_cu_main_tagged_cuda() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "tool.cu",
            "#include <cuda_runtime.h>\nint main(){return 0;}\n",
        );
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "c-main" && s.entry_path == "tool.cu")
            .expect("c-main seed for .cu missing");
        assert!(s.tags.iter().any(|t| t == "cuda"), "tags: {:?}", s.tags);
    }

    #[test]
    fn non_cuda_target_has_no_cuda_tag() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "CMakeLists.txt",
            "add_executable(plain src/main.c)\n",
        );
        write(dir.path(), "src/main.c", "int main(){return 0;}\n");
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "cmake-bin" && s.entry_command.as_deref() == Some("plain"))
            .expect("plain seed missing");
        assert!(!s.tags.iter().any(|t| t == "cuda"), "tags: {:?}", s.tags);
    }

    #[test]
    fn cmake_command_args_walker_returns_inner_text() {
        let body = "add_executable(a x.c)\nmessage(\"add_executable(fake y.c)\")\n\
                    add_executable(b \"with )paren.c\")\n";
        let args = super::cmake_command_args(body, "add_executable");
        assert_eq!(args.len(), 2, "expected two real matches, got {args:?}");
        assert_eq!(args[0], "a x.c");
        assert_eq!(args[1], "b \"with )paren.c\"");
    }

    #[test]
    fn cmake_split_args_preserves_quoted_tokens() {
        let words = super::cmake_split_args("a \"b c\" d;e [[bracket;arg]]");
        assert_eq!(words, vec!["a", "b c", "d", "e", "bracket;arg"]);
    }

    #[test]
    fn cmake_binary_with_test_like_helper_stays_binary() {
        assert!(!super::is_cmake_test_executable(
            "app",
            &["src/main.c".to_string(), "src/test_mode.c".to_string()],
        ));
    }

    #[test]
    fn cmake_target_named_tests_is_test_suite_regardless_of_sources() {
        assert!(super::is_cmake_test_executable(
            "my_tests",
            &["main.c".to_string()],
        ));
        assert!(super::is_cmake_test_executable(
            "tests",
            &["fixture.c".to_string(), "main.c".to_string()],
        ));
        assert!(super::is_cmake_test_executable("test", &[]));
        assert!(super::is_cmake_test_executable("tests", &[]));
    }

    #[test]
    fn cmake_binary_with_all_test_sources_is_test_suite() {
        assert!(super::is_cmake_test_executable(
            "my_runner",
            &["test_a.c".to_string(), "test_b.c".to_string()],
        ));
        assert!(super::is_cmake_test_executable(
            "harness",
            &[
                "test_a.c".to_string(),
                "test_b.c".to_string(),
                "shared.h".to_string(), // header — not compilable, doesn't gate
            ],
        ));
    }

    #[test]
    fn cmake_binary_with_no_compilable_sources_is_not_test() {
        assert!(!super::is_cmake_test_executable(
            "headerlib",
            &["foo.h".to_string(), "bar.hpp".to_string()],
        ));
    }

    #[test]
    fn automake_srcdir_strips_to_makefile_relative() {
        assert_eq!(
            super::expand_automake_vars("$(srcdir)/foo.c", "subdir/"),
            "foo.c"
        );
        assert_eq!(
            super::expand_automake_vars("${srcdir}/bar.cpp", "subdir/nested/"),
            "bar.cpp"
        );
        assert_eq!(super::expand_automake_vars("$(srcdir)/foo.c", ""), "foo.c");
    }

    #[test]
    fn automake_top_srcdir_left_verbatim() {
        assert_eq!(
            super::expand_automake_vars("$(top_srcdir)/include/foo.h", "subdir/"),
            "$(top_srcdir)/include/foo.h"
        );
    }

    #[test]
    fn automake_unknown_vars_pass_through() {
        assert_eq!(
            super::expand_automake_vars("$(SOURCES)/foo.c", "subdir/"),
            "$(SOURCES)/foo.c"
        );
    }

    #[test]
    fn automake_plain_path_passes_through() {
        assert_eq!(
            super::expand_automake_vars("src/main.c", "subdir/"),
            "src/main.c"
        );
    }

    #[test]
    fn cmake_project_name_last_call_wins() {
        let body =
            "project(first)\nadd_executable(a a.c)\nproject(second)\nadd_executable(b b.c)\n";
        assert_eq!(super::cmake_project_name(body).as_deref(), Some("second"));
    }

    #[test]
    fn cmake_project_name_absent_returns_none() {
        let body = "add_executable(${PROJECT_NAME} main.c)\n";
        assert!(super::cmake_project_name(body).is_none());
    }

    #[test]
    fn cmake_project_name_resolves_target_reference() {
        assert_eq!(
            super::resolve_cmake_target_name("${PROJECT_NAME}", Some("myapp")),
            "myapp"
        );
        assert_eq!(
            super::resolve_cmake_target_name("${CMAKE_PROJECT_NAME}", Some("myapp")),
            "myapp"
        );
        assert_eq!(
            super::resolve_cmake_target_name("${OTHER}", Some("myapp")),
            "${OTHER}"
        );
        assert_eq!(
            super::resolve_cmake_target_name("${PROJECT_NAME}", None),
            "${PROJECT_NAME}"
        );
    }

    #[test]
    fn cmake_add_executable_with_project_name_target() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "CMakeLists.txt",
            "project(myapp)\nadd_executable(${PROJECT_NAME} src/main.c)\n",
        );
        write(dir.path(), "src/main.c", "int main() { return 0; }\n");
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "cmake-bin")
            .expect("cmake-bin seed");
        assert_eq!(s.entry_command.as_deref(), Some("myapp"));
        assert_eq!(s.kind, FeatureKind::CliCommand);
    }

    #[test]
    fn autotools_bin_programs_expands_srcdir_var() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "subdir/Makefile.am",
            "bin_PROGRAMS = thing\nthing_SOURCES = $(srcdir)/thing.c\n",
        );
        write(dir.path(), "subdir/thing.c", "int main(){return 0;}\n");
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "autotools-bin")
            .expect("autotools-bin seed");
        assert_eq!(s.entry_path, "subdir/thing.c");
        assert!(s.owned_files.iter().any(|f| f.path == "subdir/thing.c"));
    }

    #[test]
    fn strip_cmake_comments_ignores_hash_inside_string() {
        let stripped = super::strip_cmake_comments(
            "project(x DESCRIPTION \"C# bindings\")\nadd_executable(real src/real.c)\n",
        );
        assert!(
            stripped.contains("C# bindings"),
            "hash inside a quoted string was truncated: {stripped:?}"
        );
        assert!(
            stripped.contains("add_executable(real src/real.c)"),
            "target following a hash-bearing string was dropped: {stripped:?}"
        );
    }

    #[test]
    fn cmake_hash_in_string_does_not_drop_following_target() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "CMakeLists.txt",
            "set(HASH \"sha256#abcdef\")\nadd_executable(real src/real.c)\n",
        );
        write(dir.path(), "src/real.c", "int main(){return 0;}\n");
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        assert!(
            seeds
                .iter()
                .any(|s| s.source == "cmake-bin" && s.entry_command.as_deref() == Some("real")),
            "target after a hash-in-string set() was dropped"
        );
    }

    #[test]
    fn autotools_root_bin_without_sources_has_no_phantom_path() {
        let dir = tempdir().unwrap();
        write(dir.path(), "Makefile.am", "bin_PROGRAMS = thing\n");
        let seeds = CCppMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "autotools-bin" && s.entry_command.as_deref() == Some("thing"))
            .expect("autotools-bin seed");
        assert!(
            !s.entry_path.starts_with('/'),
            "phantom leading-slash entry path: {:?}",
            s.entry_path
        );
        assert_eq!(s.entry_path, "Makefile.am");
    }
}
