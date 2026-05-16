//! C / C++ mapper: detects `main()` functions via tree-sitter, plus
//! `bin_PROGRAMS` + `lib_LTLIBRARIES` from autotools `Makefile.am` and
//! `add_executable` + `add_library` from CMake. Headers in `include/` and
//! sibling `*.h` files get pulled into the corresponding library feature
//! as context.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use anyhow::Result;
use codesage_protocol::{FeatureConfidence, FeatureKind, Language};
use regex::Regex;
use tree_sitter::Parser;

use crate::mappers::shared::{is_safe_file, walk_files};
use crate::mappers::types::{FeatureMapper, FeatureSeed, SeedFile};

/// Single mapper that emits both C and C++ seeds; the language tag is
/// chosen per-file based on extension. Run only once per repo, regardless
/// of whether the repo is "C" or "C++" or mixed.
pub struct CCppMapper;

impl FeatureMapper for CCppMapper {
    fn name(&self) -> &'static str {
        "c-cpp"
    }
    fn map(&self, root: &Path) -> Result<Vec<FeatureSeed>> {
        // Skip work if the repo has no C/C++ files at all.
        let files: Vec<String> = walk_files(root, root, 50_000)
            .into_iter()
            .filter(|p| is_c_or_cpp_source(p) || is_makefile(p) || is_cmake(p))
            .collect();
        if files.is_empty() {
            return Ok(Vec::new());
        }
        let mut seeds: Vec<FeatureSeed> = Vec::new();
        seeds.extend(autotools_targets(root, &files)?);
        seeds.extend(cmake_targets(root, &files)?);
        // Populate the seen-set from already-declared build targets BEFORE
        // running main() detection. Without this, a binary declared in
        // CMakeLists.txt (or Makefile.am) AND containing `main()` produces
        // two duplicate features — same entry, same kind, different
        // `source`. The `dedup_by_entry` pass keyed on source preserved
        // both; populate the seen-set so main() detection skips paths
        // already claimed by a build-system target (CR-005).
        let already_seeded_paths: BTreeSet<String> = seeds
            .iter()
            .filter(|s| s.kind == FeatureKind::CliCommand)
            .map(|s| s.entry_path.clone())
            .collect();
        seeds.extend(main_function_targets(root, &files, &already_seeded_paths)?);
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

fn autotools_targets(root: &Path, files: &[String]) -> Result<Vec<FeatureSeed>> {
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
                let language = lang_for_path(&entry);
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
                    owned_files: sources
                        .iter()
                        .map(|s| SeedFile {
                            path: prefix_dir(&dir, s),
                            reason: "target source".to_string(),
                        })
                        .collect(),
                    context_files: vec![SeedFile {
                        path: mf.to_string(),
                        reason: "build target declaration".to_string(),
                    }],
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
                let language = lang_for_path(&entry);
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
                    owned_files: sources
                        .iter()
                        .map(|s| SeedFile {
                            path: prefix_dir(&dir, s),
                            reason: "target source".to_string(),
                        })
                        .collect(),
                    context_files: vec![SeedFile {
                        path: mf.to_string(),
                        reason: "build target declaration".to_string(),
                    }],
                    tests: Vec::new(),
                    test_prefixes: vec![format!("{}tests", dir)],
                });
            }
        }
    }
    Ok(out)
}

fn cmake_targets(root: &Path, files: &[String]) -> Result<Vec<FeatureSeed>> {
    let mut out: Vec<FeatureSeed> = Vec::new();
    let cmake_files: Vec<&String> = files.iter().filter(|f| is_cmake(f)).collect();
    if cmake_files.is_empty() {
        return Ok(out);
    }
    let exe_re = Regex::new(r"(?ms)add_executable\s*\(\s*([A-Za-z_][\w\-]*)\s+([^)]*)\)")?;
    let lib_re = Regex::new(
        r"(?ms)add_library\s*\(\s*([A-Za-z_][\w\-]*)(?:\s+(?:SHARED|STATIC|MODULE|OBJECT|INTERFACE))?\s+([^)]*)\)",
    )?;
    for cm in cmake_files {
        let path = root.join(cm);
        let body = fs::read_to_string(&path).unwrap_or_default();
        let dir = parent_dir(cm);
        for cap in exe_re.captures_iter(&body) {
            let name = cap
                .get(1)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let sources_str = cap
                .get(2)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            if !is_valid_target_name(&name) {
                continue;
            }
            let sources: Vec<String> = sources_str
                .split_whitespace()
                .filter(|s| is_c_or_cpp_compilable(s))
                .map(|s| s.to_string())
                .collect();
            let entry = pick_entry(root, &dir, &sources, &name).unwrap_or_else(|| cm.to_string());
            let language = lang_for_path(&entry);
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
                owned_files: sources
                    .iter()
                    .map(|s| SeedFile {
                        path: prefix_dir(&dir, s),
                        reason: "target source".to_string(),
                    })
                    .collect(),
                context_files: vec![SeedFile {
                    path: cm.to_string(),
                    reason: "CMake target declaration".to_string(),
                }],
                tests: Vec::new(),
                test_prefixes: vec![format!("{}tests", dir)],
            });
        }
        for cap in lib_re.captures_iter(&body) {
            let name = cap
                .get(1)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let sources_str = cap
                .get(2)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            if !is_valid_target_name(&name) {
                continue;
            }
            let sources: Vec<String> = sources_str
                .split_whitespace()
                .filter(|s| is_c_or_cpp_compilable(s))
                .map(|s| s.to_string())
                .collect();
            let entry = pick_entry(root, &dir, &sources, &name).unwrap_or_else(|| cm.to_string());
            let language = lang_for_path(&entry);
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
                owned_files: sources
                    .iter()
                    .map(|s| SeedFile {
                        path: prefix_dir(&dir, s),
                        reason: "target source".to_string(),
                    })
                    .collect(),
                context_files: vec![SeedFile {
                    path: cm.to_string(),
                    reason: "CMake target declaration".to_string(),
                }],
                tests: Vec::new(),
                test_prefixes: vec![format!("{}tests", dir)],
            });
        }
    }
    Ok(out)
}

fn main_function_targets(
    root: &Path,
    files: &[String],
    already_seeded: &BTreeSet<String>,
) -> Result<Vec<FeatureSeed>> {
    let mut out: Vec<FeatureSeed> = Vec::new();
    let mut c_parser = Parser::new();
    let mut cpp_parser = Parser::new();
    c_parser.set_language(&tree_sitter_c::LANGUAGE.into())?;
    cpp_parser.set_language(&tree_sitter_cpp::LANGUAGE.into())?;
    for rel in files.iter().filter(|p| is_c_or_cpp_compilable(p)) {
        if already_seeded.contains(rel) {
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

/// Dedup keyed on `(entry_path, kind)`. The earlier key included `source`,
/// so two seeds emitted by different mappers (cmake-bin vs c-main) for the
/// same entry survived as separate features. The orchestrator stored both
/// under different `feature_id`s and `assess_risk` ended up double-counting
/// the file in feature listings. First seed wins — autotools/CMake run
/// before main() detection so the high-confidence build-target seed is
/// preserved when both fire (CR-005).
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
        let seeds = CCppMapper.map(dir.path()).unwrap();
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
        let seeds = CCppMapper.map(dir.path()).unwrap();
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
        let seeds = CCppMapper.map(dir.path()).unwrap();
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
        let seeds = CCppMapper.map(dir.path()).unwrap();
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
        let seeds = CCppMapper.map(dir.path()).unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "autotools-bin")
            .expect("autotools-bin seed");
        assert_eq!(s.entry_command.as_deref(), Some("thing"));
        assert_eq!(s.kind, FeatureKind::CliCommand);
    }

    #[test]
    fn cmake_bin_with_main_function_emits_single_feature() {
        // CR-005 regression: a binary declared in CMakeLists.txt that
        // also contains `int main()` was previously seeded twice — once
        // as `cmake-bin`, once as `c-main` — because the seen-set wasn't
        // populated from build-target paths and the dedup key included
        // `source`. The build-target seed must win.
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "CMakeLists.txt",
            "add_executable(myapp src/main.c)\n",
        );
        write(dir.path(), "src/main.c", "int main(){return 0;}\n");
        let seeds = CCppMapper.map(dir.path()).unwrap();
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
        // CR-005 regression for the autotools path: same scenario as
        // CMake but anchored on Makefile.am.
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "Makefile.am",
            "bin_PROGRAMS = thing\nthing_SOURCES = thing.c\n",
        );
        write(dir.path(), "thing.c", "int main(){return 0;}\n");
        let seeds = CCppMapper.map(dir.path()).unwrap();
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
}
