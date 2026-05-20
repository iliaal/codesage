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

/// Path looks like a C/C++ test fixture under conventional naming:
/// directory under `tests/` / `test/` / `__tests__/`; basename prefixed
/// with `test_` or `test-`; basename suffixed `_tests.cpp` / `-test.c`;
/// `FooTest.cpp` / `BarTests.cc`. Mirrors clawpatch's `isCOrCppTestPath`
/// so the same shape we use to skip `main()` detection in `tests/`
/// drives CMake test-target classification too.
fn is_c_or_cpp_test_path(rel: &str) -> bool {
    let base = rel.rsplit('/').next().unwrap_or(rel);
    let lower = rel.to_ascii_lowercase();
    if lower.starts_with("test/")
        || lower.starts_with("tests/")
        || lower.contains("/test/")
        || lower.contains("/tests/")
        || lower.contains("/__tests__/")
    {
        return true;
    }
    let base_lower = base.to_ascii_lowercase();
    if base_lower.starts_with("test_") || base_lower.starts_with("test-") {
        return true;
    }
    // foo_test.c / foo-tests.cpp
    let stem = base.rsplit_once('.').map(|(s, _)| s).unwrap_or(base);
    let stem_lower = stem.to_ascii_lowercase();
    if stem_lower.ends_with("_test")
        || stem_lower.ends_with("-test")
        || stem_lower.ends_with("_tests")
        || stem_lower.ends_with("-tests")
    {
        return true;
    }
    // FooTest.cpp / BarTests.cc (case-sensitive Test/Tests suffix on the
    // *original* stem so `foo_test.c` doesn't double-match here).
    if stem.ends_with("Test") || stem.ends_with("Tests") {
        return true;
    }
    false
}

/// Classify a CMake `add_executable(...)` target as a test suite when
/// either the target name ends in `tests?` (with optional `_` or `-`
/// separator, case-insensitive) or any of its sources is a test path.
/// Matches clawpatch commit f21b76c's `isCMakeTestExecutableTarget`.
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
    sources.iter().any(|s| is_c_or_cpp_test_path(s))
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
    // CMake parsing uses an explicit walker (`cmake_command_args` + friends)
    // rather than regexes. Two reasons: (a) the previous `[^)]*` source-list
    // capture truncated targets whose quoted args contained `)` (common with
    // generator expressions like `$<$<CONFIG:Debug>:debug.c>`); (b) the
    // walker skips `command(...)` text inside quoted strings and bracket
    // arguments, so `message("add_executable(fake)")` no longer leaks a
    // spurious feature. Ported from clawpatch commit 162a6fe.
    //
    // Target name shape (`[A-Za-z0-9_.\-]+` with no leading `$` / `\\` / `(`
    // / `=` / `#`) and the case-insensitivity of CMake command names are
    // enforced by `is_valid_target_name` and `cmake_command_args` rather
    // than by a regex.
    for cm in cmake_files {
        let path = root.join(cm);
        let raw = fs::read_to_string(&path).unwrap_or_default();
        let body = strip_cmake_comments(&raw);
        let dir = parent_dir(cm);

        // Collect late-bound `target_sources(name [PRIVATE|PUBLIC|INTERFACE] …)`
        // additions first so the target loop below can merge them when it
        // sees the matching `add_executable` / `add_library`.
        let mut extra_sources: HashMap<String, Vec<String>> = HashMap::new();
        for args in cmake_command_args(&body, "target_sources") {
            let mut words = cmake_split_args(&args);
            if words.is_empty() {
                continue;
            }
            let name = words.remove(0);
            // Real CMake requires a PRIVATE|PUBLIC|INTERFACE scope keyword
            // here; tolerate its absence rather than skipping the call, but
            // drop the keyword if present so a phantom "PRIVATE" source
            // never lands in `extra_sources`.
            strip_target_sources_scope(&mut words);
            extra_sources.entry(name).or_default().extend(words);
        }

        for args in cmake_command_args(&body, "add_executable") {
            let mut words = cmake_split_args(&args);
            if words.is_empty() {
                continue;
            }
            let name = words.remove(0);
            if !is_valid_target_name(&name) {
                continue;
            }
            let mut all_sources: Vec<String> = words;
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
            // Test-target classification: name ending in `tests?` (with
            // optional separator) or any source under a test path means
            // this `add_executable` is a test harness, not a shippable
            // CLI. Emit one `cmake-test` test-suite seed in that case so
            // `recommend_tests` and PR-level risk views pick it up
            // correctly; otherwise emit the regular `cmake-bin` cli seed.
            if is_cmake_test_executable(&name, &all_sources) {
                let lang_tag = if language == Language::Cpp {
                    "cpp"
                } else {
                    "c"
                };
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
                    title: format!("CMake test suite `{name}`"),
                    summary: format!("CMake test executable `{name}` declared in {cm}"),
                    kind: FeatureKind::TestSuite,
                    source: "cmake-test",
                    confidence: FeatureConfidence::High,
                    entry_path: entry,
                    entry_symbol: None,
                    entry_route: None,
                    entry_command: None,
                    test_command: None,
                    language,
                    tags: vec![lang_tag.to_string(), "test".to_string()],
                    owned_files,
                    context_files,
                    tests,
                    test_prefixes: vec![format!("{}tests", dir)],
                });
                continue;
            }
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
        for args in cmake_command_args(&body, "add_library") {
            let mut words = cmake_split_args(&args);
            if words.is_empty() {
                continue;
            }
            let name = words.remove(0);
            if !is_valid_target_name(&name) {
                continue;
            }
            // `add_library(name [SHARED|STATIC|MODULE|OBJECT|INTERFACE] …)`
            // — the library-type keyword, if present, is not a source.
            strip_library_type_keyword(&mut words);
            let mut all_sources: Vec<String> = words;
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
/// preserved so the line layout isn't compressed.
///
/// The walk is driven by byte indices because the bracket-close marker
/// (`]=*]`, `\n`, `#`, `[`) is pure ASCII and so can never match inside
/// a UTF-8 continuation byte (continuation bytes are 0x80..=0xBF). Non-
/// comment runs are copied through as whole `char`s so multibyte
/// scalars round-trip — copying byte-by-byte via `b as char` produced
/// mojibake on any non-ASCII identifier or path in `CMakeLists.txt`.
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
        if bytes[i] == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Copy one Unicode scalar verbatim. For ASCII this is one byte;
        // for multibyte sequences we slice on the char boundary and
        // append the full scalar, never `byte as char`.
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
                // open+1 and close are both ASCII byte positions so
                // slicing the str at those indices stays on char
                // boundaries.
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
            // Recompute the opener length (`[`, `=*`, `[`) to know where
            // the content starts and the closer length to know where it
            // ends. Bracket-arg content is raw — no escape processing.
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

/// `\X` → `X` for any single character. CMake's quoted-string rules
/// allow `\\`, `\"`, `\n`, etc. — we don't expand the special letter
/// escapes (we keep `\n` as a literal `n`) because the only thing this
/// mapper does with the result is path resolution, where preserving the
/// literal beats interpreting it.
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
    if let Some(first) = words.first() {
        let kw = first.to_ascii_uppercase();
        if matches!(kw.as_str(), "PRIVATE" | "PUBLIC" | "INTERFACE") {
            words.remove(0);
        }
    }
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

    #[test]
    fn cmake_test_target_emits_test_suite_not_cli() {
        // `add_executable(foo_tests test_a.cpp test_b.cpp)` is a unit-test
        // harness, not a shippable CLI; emit as `cmake-test` /
        // FeatureKind::TestSuite so it lands in `recommend_tests` and
        // doesn't pollute `list_features --kind cli-command`.
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
        // Even with a neutral target name, sources under `tests/` flip
        // the classification — `add_executable(runner tests/main.c)` is
        // a test harness, not a CLI command.
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
        // Negative regression: ordinary names with ordinary sources keep
        // their pre-fix `cmake-bin` classification.
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
    fn is_c_or_cpp_test_path_covers_documented_patterns() {
        // Patterns mirror clawpatch's `isCOrCppTestPath`. Listed here so
        // a future change that loosens the helper trips the test.
        for path in [
            "tests/main.c",
            "test/main.cpp",
            "src/__tests__/widget.cpp",
            "src/test_widget.c",
            "src/test-widget.c",
            "src/widget_test.c",
            "src/widget-test.c",
            "src/widget_tests.cpp",
            "src/WidgetTest.cpp",
            "src/WidgetTests.cc",
        ] {
            assert!(super::is_c_or_cpp_test_path(path), "should match: {path}");
        }
        for path in [
            "src/main.c",
            "src/widget.cpp",
            "include/widget.h",
            "src/testimony.c", // `test` prefix but not separator-followed
        ] {
            assert!(
                !super::is_c_or_cpp_test_path(path),
                "should NOT match: {path}"
            );
        }
    }

    #[test]
    fn strip_cmake_comments_preserves_non_ascii_paths() {
        // Regression for fnd_2fbb868e: byte-by-byte `b as char` mangled
        // multibyte UTF-8 source paths into Latin-1 codepoints, so any
        // CMakeLists.txt with a non-ASCII source path produced mojibake
        // and the resulting cmake-bin features couldn't be matched back
        // to the real file.
        let stripped = super::strip_cmake_comments("add_executable(app src/café.c)\n");
        assert!(
            stripped.contains("café"),
            "non-ASCII path lost UTF-8 round-trip: {stripped:?}"
        );
    }

    #[test]
    fn cmake_ignores_command_text_inside_strings() {
        // Ported from clawpatch 162a6fe: `message("add_executable(...)")`
        // and `message([[add_library(...)]])` must not surface as
        // features. The walker skips command-like text inside quoted
        // strings and bracket arguments; the old `[^)]*` regex had no
        // notion of string scope and matched right through them.
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
        // Ported from clawpatch 162a6fe: a quoted source path with an
        // embedded space stays one word, not two. The old regex relied
        // on `split_whitespace`, which would split "src/main file.cpp"
        // into "src/main" and "file.cpp" and emit phantom sources.
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
        // The headline regression: the old `[^)]*` capture truncated
        // the source list at the first `)`, so a quoted path containing
        // `)` (e.g., a vendored legacy file named `foo(v1).cpp`) made
        // the regex eat only the prefix and the rest of the line stayed
        // unparsed. The walker treats the `)` as ordinary content
        // because it sits inside `"..."`.
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
        // `add_library(name SHARED src/a.c)` must not record "SHARED" as
        // a phantom owned file. The old regex consumed the type keyword
        // via a non-capturing group; the new walker has to strip it
        // explicitly after splitting words.
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
    fn cmake_command_args_walker_returns_inner_text() {
        // Direct unit test for the walker so regressions in the parser
        // primitive can be diagnosed without round-tripping through the
        // full mapper.
        let body = "add_executable(a x.c)\nmessage(\"add_executable(fake y.c)\")\n\
                    add_executable(b \"with )paren.c\")\n";
        let args = super::cmake_command_args(body, "add_executable");
        assert_eq!(args.len(), 2, "expected two real matches, got {args:?}");
        assert_eq!(args[0], "a x.c");
        assert_eq!(args[1], "b \"with )paren.c\"");
    }

    #[test]
    fn cmake_split_args_preserves_quoted_tokens() {
        // Quoted words survive whitespace and `;`; unquoted words split
        // on `;` per CMake list semantics.
        let words = super::cmake_split_args("a \"b c\" d;e [[bracket;arg]]");
        assert_eq!(words, vec!["a", "b c", "d", "e", "bracket;arg"]);
    }
}
