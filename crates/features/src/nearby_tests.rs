//! Test-file discovery from the file list, by language convention.
//!
//! Adopted from clawpatch's `nearbyTests` (src/mappers/shared.ts) but
//! works against an already-walked path list rather than re-walking the
//! tree per seed. Caller passes the full file inventory once and we filter.
//!
//! Conventions covered:
//!
//! - Rust: sibling `tests/*.rs` to the entry's package dir.
//! - PHP: `*Test.php` / `*_test.php` siblings, plus the file matching the
//!   class name suffix in standard `tests/` directories. Also `.phpt`
//!   under any `tests/` directory tied to the entry's dir.
//! - Python: `tests/test_<stem>.py`, `tests/<stem>_test.py`, sibling
//!   `test_<stem>.py`.
//! - JS / TS: sibling `*.{test,spec}.{ts,tsx,js,jsx}` and `__tests__/*`.
//! - Go: `*_test.go` in the same directory.
//! - C / C++: `tests/<stem>.c` or `tests/<stem>_test.c` (best-effort).
//! - Java: `*Test.java` / `*Tests.java` siblings and standard `src/test/` paths.

use crate::mappers::types::FeatureSeed;
use codesage_protocol::Language;

const MAX_TESTS_PER_SEED: usize = 5;

/// Test-shaped subset of the repo file inventory, classified once per map
/// run. Per-seed discovery then scans only this (much smaller) list instead
/// of re-running the any-language shape check over every repo file for every
/// seed — the hot path on seed-dense repos (php-src maps one seed per
/// function against a ~20k-file inventory).
pub struct TestFileIndex<'a> {
    /// Sorted. In production the walk output is already sorted, so sorting
    /// here is a no-op that makes the prefix range-scan valid for any caller.
    test_shaped: Vec<&'a str>,
}

impl<'a> TestFileIndex<'a> {
    pub fn build(all_files: &'a [String]) -> Self {
        let mut test_shaped: Vec<&'a str> = all_files
            .iter()
            .filter(|f| is_test_file_any_language(f))
            .map(|f| f.as_str())
            .collect();
        test_shaped.sort_unstable();
        Self { test_shaped }
    }

    /// Contiguous run of test-shaped files under `pfx_slash` (which must end
    /// in `/`), found by binary search — O(log n + hits) per declared prefix
    /// instead of a full-inventory scan.
    fn with_dir_prefix(&self, pfx_slash: &str) -> &[&'a str] {
        let lo = self.test_shaped.partition_point(|f| *f < pfx_slash);
        let run = self.test_shaped[lo..].partition_point(|f| f.starts_with(pfx_slash));
        &self.test_shaped[lo..lo + run]
    }

    fn contains_exact(&self, path: &str) -> bool {
        self.test_shaped.binary_search(&path).is_ok()
    }
}

pub fn nearby_tests(seed: &FeatureSeed, all_files: &[String]) -> Vec<String> {
    nearby_tests_indexed(seed, &TestFileIndex::build(all_files))
}

pub fn nearby_tests_indexed(seed: &FeatureSeed, index: &TestFileIndex) -> Vec<String> {
    let stem = file_stem(&seed.entry_path);
    let dir = parent_dir(&seed.entry_path);

    let mut out: Vec<String> = Vec::new();

    // Authoritative first: tests the mapper explicitly declared via
    // `test_prefixes` (e.g. an autotools `<dir>/tests` prefix). These run BEFORE
    // the heuristic convention/stem scan so loose matches can't starve them —
    // pre-reorder, five stem matches could fill the cap and drop every declared
    // test. The declared dir is matched by test SHAPE in any language, not only
    // the seed's: a C-language PHP-extension seed declares a dir of .phpt tests
    // that a seed-language filter would reject wholesale. Within the declared
    // dirs, the seed's OWN-language tests take the cap slots first — otherwise
    // alphabetically-earlier files of another language (five test_*.py before
    // the crate's *.rs integration tests) exhaust the cap and starve the tests
    // the seed actually runs.
    let mut lang_hits: Vec<&str> = Vec::new();
    let mut other_hits: Vec<&str> = Vec::new();
    for prefix in &seed.test_prefixes {
        if lang_hits.len() >= MAX_TESTS_PER_SEED {
            break;
        }
        let pfx = prefix.trim_end_matches('/');
        if pfx.is_empty() {
            continue;
        }
        let pfx_slash = format!("{pfx}/");
        let exact = if index.contains_exact(pfx) {
            Some(pfx)
        } else {
            None
        };
        for &f in exact.iter().chain(index.with_dir_prefix(&pfx_slash)) {
            if lang_hits.contains(&f) || other_hits.contains(&f) {
                continue;
            }
            if matches_language(f, seed.language) && is_test_file(f, seed.language) {
                lang_hits.push(f);
            } else {
                other_hits.push(f);
            }
        }
    }
    for f in lang_hits.into_iter().chain(other_hits) {
        if out.len() >= MAX_TESTS_PER_SEED {
            break;
        }
        out.push(f.to_string());
    }

    // Heuristic convention/stem discovery fills the remaining slots from the
    // seed-language candidates. Index order matches walk order, so
    // cap-truncation picks the same winners a full re-scan would.
    let candidates = index
        .test_shaped
        .iter()
        .filter(|f| matches_language(f, seed.language) && is_test_file(f, seed.language));
    for f in candidates {
        if out.len() >= MAX_TESTS_PER_SEED {
            break;
        }
        if *f == seed.entry_path {
            continue;
        }
        if out.iter().any(|x| x == f) {
            continue;
        }
        if file_relates_to(seed, stem, &dir, f) {
            out.push(f.to_string());
        }
    }

    out.sort();
    out.dedup();
    out.truncate(MAX_TESTS_PER_SEED);
    out
}

fn file_relates_to(seed: &FeatureSeed, stem: &str, dir: &str, candidate: &str) -> bool {
    // 1. Sibling directory: same parent dir.
    let cand_dir = parent_dir(candidate);
    if !dir.is_empty() && cand_dir == dir {
        return true;
    }
    // 2. Test file whose stem matches the entry's stem at a word boundary,
    // but only within the same monorepo package/crate root so identical
    // stems in different packages (e.g. packages/api vs packages/ui) don't
    // cross-attach.
    let cand_stem = file_stem(candidate);
    if stem_at_word_boundary(cand_stem, stem)
        && same_stem_scope(seed.entry_path.as_str(), candidate)
    {
        return true;
    }
    // 3. Convention dirs at the repo root that are typically the test home.
    // Rust integration tests in `tests/` always attach (no stem match needed
    // — any test in that dir exercises the crate as a whole). For other
    // languages we require a stem match to avoid attaching every test in the
    // suite to every entrypoint.
    let convention_dirs: &[&str] = match seed.language {
        Language::Rust => &["tests"],
        Language::Php => &["tests", "test"],
        Language::Python => &["tests", "test"],
        Language::Go => &[],
        Language::Java => &["src/test", "test", "tests"],
        Language::JavaScript | Language::TypeScript => &["__tests__", "tests"],
        Language::C | Language::Cpp => &["tests", "test"],
    };
    let always_attach_in_convention_dir = matches!(seed.language, Language::Rust);
    for default in convention_dirs {
        if !path_has_dir_prefix(candidate, default) {
            continue;
        }
        if always_attach_in_convention_dir {
            return true;
        }
        if stem_match(candidate, stem) && same_stem_scope(seed.entry_path.as_str(), candidate) {
            return true;
        }
    }
    false
}

fn stem_match(candidate: &str, stem: &str) -> bool {
    stem_at_word_boundary(file_stem(candidate), stem)
}

/// True if `stem` occurs in `cand_stem` as a prefix or suffix at a word
/// boundary — the adjacent character is a separator (`_` / `.` / `-`), an
/// uppercase letter (a CamelCase word start), or the string edge. A bare
/// substring/prefix match attaches unrelated files: entry stem `main` otherwise
/// matches `maintenance_test`. The boundary keeps the real conventions
/// (`main_test`, `test_main`, `MainTest`) while rejecting `maintenance`.
fn stem_at_word_boundary(cand_stem: &str, stem: &str) -> bool {
    if stem.is_empty() || cand_stem.len() < stem.len() {
        return false;
    }
    // A lowercase letter or digit continues a word; anything else (separator,
    // uppercase, edge) is a boundary.
    let is_word = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    // Prefix: stem at the start, followed by a boundary.
    if let Some(rest) = cand_stem.strip_prefix(stem) {
        match rest.chars().next() {
            None => return true,
            Some(c) if !is_word(c) => return true,
            _ => {}
        }
    }
    // Suffix: stem at the end, preceded by a boundary, or the stem itself begins
    // a CamelCase word (its first char is uppercase, e.g. `...Foo` for `Foo`).
    if let Some(head) = cand_stem.strip_suffix(stem) {
        let stem_starts_upper = stem.chars().next().is_some_and(|c| c.is_ascii_uppercase());
        match head.chars().last() {
            None => return true,
            Some(c) if !is_word(c) => return true,
            _ if stem_starts_upper => return true,
            _ => {}
        }
    }
    false
}

/// Test-shaped in ANY language: the file carries some language's extension and
/// that language's test conventions accept it. Requiring a language match keeps
/// docs and fixture data out of declared test dirs while still accepting
/// cross-language suites (.phpt under a C extension's tests dir).
pub(crate) fn is_test_file_any_language(path: &str) -> bool {
    const ALL: [Language; 9] = [
        Language::Rust,
        Language::Php,
        Language::Python,
        Language::Go,
        Language::Java,
        Language::JavaScript,
        Language::TypeScript,
        Language::C,
        Language::Cpp,
    ];
    ALL.iter()
        .any(|&l| matches_language(path, l) && is_test_file(path, l))
}

fn matches_language(path: &str, language: Language) -> bool {
    match language {
        Language::Rust => path.ends_with(".rs"),
        Language::Php => path.ends_with(".php") || path.ends_with(".phpt"),
        Language::Python => path.ends_with(".py"),
        Language::Go => path.ends_with(".go"),
        Language::Java => path.ends_with(".java"),
        Language::JavaScript => {
            path.ends_with(".js") || path.ends_with(".jsx") || path.ends_with(".mjs")
        }
        Language::TypeScript => path.ends_with(".ts") || path.ends_with(".tsx"),
        Language::C => path.ends_with(".c") || path.ends_with(".h"),
        Language::Cpp => {
            path.ends_with(".cpp")
                || path.ends_with(".cc")
                || path.ends_with(".cxx")
                || path.ends_with(".hpp")
                || path.ends_with(".hh")
                || path.ends_with(".hxx")
        }
    }
}

/// Canonical per-language test-shape predicate. Mappers must route their
/// own "is this a test file?" checks through this (or the broader
/// [`is_test_file_any_language`]) instead of re-encoding the conventions.
pub(crate) fn is_test_file(path: &str, language: Language) -> bool {
    match language {
        Language::Rust => {
            path.starts_with("tests/") || path.contains("/tests/") || path.ends_with("_test.rs")
        }
        Language::Php => {
            path.ends_with("Test.php")
                || path.ends_with("_test.php")
                || path.ends_with(".phpt")
                || path.starts_with("tests/")
                || path.contains("/tests/")
                || path.contains("/test/")
        }
        Language::Python => {
            is_python_test_basename(path) || path.starts_with("tests/") || path.contains("/tests/")
        }
        Language::Go => path.ends_with("_test.go"),
        Language::Java => {
            path.ends_with("Test.java")
                || path.ends_with("Tests.java")
                || path.starts_with("src/test/")
                || path.contains("/src/test/")
                || path.starts_with("test/")
                || path.starts_with("tests/")
        }
        Language::JavaScript | Language::TypeScript => {
            path.ends_with(".test.ts")
                || path.ends_with(".test.tsx")
                || path.ends_with(".test.js")
                || path.ends_with(".test.jsx")
                || path.ends_with(".test.mts")
                || path.ends_with(".test.cts")
                || path.ends_with(".test.mjs")
                || path.ends_with(".test.cjs")
                || path.ends_with(".spec.ts")
                || path.ends_with(".spec.tsx")
                || path.ends_with(".spec.js")
                || path.ends_with(".spec.jsx")
                || path.starts_with("__tests__/")
                || path.contains("/__tests__/")
                || path.starts_with("tests/")
        }
        Language::C | Language::Cpp => is_c_or_cpp_test_path(path),
    }
}

/// The `test_*.py` / `*_test.py` basename convention, single-sourced so the
/// Python mapper's pytest-file classifier and this module's Python arm can't
/// drift apart. Callers guard the `.py` extension themselves.
pub(crate) fn is_python_test_basename(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    base.starts_with("test_") || base.ends_with("_test.py")
}

/// Path looks like a C/C++ test fixture under conventional naming:
/// directory under `tests/` / `test/` / `__tests__/` (any depth,
/// case-insensitive); basename prefixed with `test_` or `test-`; basename
/// suffixed `_tests.cpp` / `-test.c`; `FooTest.cpp` / `BarTests.cc`.
/// Mirrors clawpatch's `isCOrCppTestPath`. The same shape drives CMake
/// test-target classification, `main()`-suppression in the c-main walker,
/// and this module's C/C++ test association.
pub(crate) fn is_c_or_cpp_test_path(rel: &str) -> bool {
    let base = rel.rsplit('/').next().unwrap_or(rel);
    let lower = rel.to_ascii_lowercase();
    if lower.starts_with("test/")
        || lower.starts_with("tests/")
        || lower.starts_with("__tests__/")
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

/// When both paths live under `packages/<name>/` or `crates/<name>/`, require
/// the same `<name>`. Non-monorepo layouts keep the historical cross-dir
/// stem-match behavior (e.g. `acme/widget.py` + `other/widget_test.py`).
fn same_stem_scope(entry: &str, candidate: &str) -> bool {
    match (monorepo_member_root(entry), monorepo_member_root(candidate)) {
        (Some(e), Some(c)) => e == c,
        (Some(_), None) | (None, Some(_)) => false,
        (None, None) => true,
    }
}

fn monorepo_member_root(rel: &str) -> Option<String> {
    for prefix in ["packages/", "crates/"] {
        // `continue` on a non-matching prefix — a bare `?` here returned None
        // from the whole function on the first miss, leaving the `crates/`
        // branch dead so `crates/*` sources never resolved a member root.
        let Some(rest) = rel.strip_prefix(prefix) else {
            continue;
        };
        let member = rest.split('/').next().unwrap_or("");
        if !member.is_empty() {
            return Some(format!("{prefix}{member}"));
        }
    }
    None
}

/// True if `dir` is the first path segment of `path` (i.e. `path` starts with
/// `dir/`). Avoids the `format!("{dir}/")` allocation on the per-candidate hot
/// path.
fn path_has_dir_prefix(path: &str, dir: &str) -> bool {
    path.len() > dir.len() && path.as_bytes()[dir.len()] == b'/' && path.starts_with(dir)
}

fn parent_dir(rel: &str) -> String {
    match rel.rfind('/') {
        Some(i) => rel[..i].to_string(),
        None => String::new(),
    }
}

fn file_stem(rel: &str) -> &str {
    let base = rel.rsplit('/').next().unwrap_or(rel);
    base.split('.').next().unwrap_or(base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codesage_protocol::FeatureKind;

    fn seed(entry: &str, language: Language) -> FeatureSeed {
        FeatureSeed {
            source: "test",
            ..FeatureSeed::new(FeatureKind::CliCommand, language, "", entry)
        }
    }

    #[test]
    fn is_c_or_cpp_test_path_covers_documented_patterns() {
        // Patterns mirror clawpatch's `isCOrCppTestPath`. Listed here so
        // a future change that loosens the helper trips the test.
        for path in [
            "tests/main.c",
            "test/main.cpp",
            "__tests__/widget.c",
            "src/__tests__/widget.cpp",
            "src/test_widget.c",
            "src/test-widget.c",
            "src/widget_test.c",
            "src/widget-test.c",
            "src/widget_tests.cpp",
            "src/WidgetTest.cpp",
            "src/WidgetTests.cc",
        ] {
            assert!(is_c_or_cpp_test_path(path), "should match: {path}");
        }
        for path in [
            "src/main.c",
            "src/widget.cpp",
            "include/widget.h",
            "src/testimony.c", // `test` prefix but not separator-followed
        ] {
            assert!(!is_c_or_cpp_test_path(path), "should NOT match: {path}");
        }
    }

    #[test]
    fn rust_integration_test_found() {
        let s = seed("src/main.rs", Language::Rust);
        let all = vec![
            "src/main.rs".to_string(),
            "tests/integration.rs".to_string(),
            "tests/other.rs".to_string(),
        ];
        let t = nearby_tests(&s, &all);
        assert!(t.contains(&"tests/integration.rs".to_string()));
    }

    #[test]
    fn php_unit_test_found_by_stem() {
        let s = seed("src/UserService.php", Language::Php);
        let all = vec![
            "src/UserService.php".to_string(),
            "tests/UserServiceTest.php".to_string(),
            "tests/OtherTest.php".to_string(),
        ];
        let t = nearby_tests(&s, &all);
        assert!(t.contains(&"tests/UserServiceTest.php".to_string()));
    }

    #[test]
    fn python_test_with_test_prefix() {
        let s = seed("acme/handler.py", Language::Python);
        let all = vec![
            "acme/handler.py".to_string(),
            "tests/test_handler.py".to_string(),
        ];
        let t = nearby_tests(&s, &all);
        assert!(t.contains(&"tests/test_handler.py".to_string()));
    }

    #[test]
    fn go_sibling_test_found() {
        let s = seed("internal/auth/login.go", Language::Go);
        let all = vec![
            "internal/auth/login.go".to_string(),
            "internal/auth/login_test.go".to_string(),
        ];
        let t = nearby_tests(&s, &all);
        assert!(t.contains(&"internal/auth/login_test.go".to_string()));
    }

    #[test]
    fn cap_at_5_per_seed() {
        let s = seed("src/foo.rs", Language::Rust);
        let mut all = vec!["src/foo.rs".to_string()];
        for i in 0..10 {
            all.push(format!("tests/foo{i}.rs"));
        }
        let t = nearby_tests(&s, &all);
        assert!(t.len() <= 5);
    }

    #[test]
    fn substring_stem_does_not_attach_unrelated_test() {
        // entry stem "main" must NOT match "maintenance_test" — that's a bare
        // substring, not a word-boundary match. The noise file is outside the
        // entry's dir and any convention dir so only rule 2 could attach it.
        let s = seed("src/app/main.py", Language::Python);
        let all = vec![
            "src/app/main.py".to_string(),
            "src/util/maintenance_test.py".to_string(),
        ];
        let t = nearby_tests(&s, &all);
        assert!(
            !t.contains(&"src/util/maintenance_test.py".to_string()),
            "substring stem match should not attach: {t:?}"
        );
    }

    #[test]
    fn separator_boundary_stem_still_attaches() {
        // Word-boundary matches that ARE real conventions must still attach,
        // even outside the entry's directory (so this isn't a rule-1 match).
        let s = seed("acme/widget.py", Language::Python);
        let all = vec![
            "acme/widget.py".to_string(),
            "other/widget_test.py".to_string(),
        ];
        let t = nearby_tests(&s, &all);
        assert!(
            t.contains(&"other/widget_test.py".to_string()),
            "separator-boundary stem match should attach: {t:?}"
        );
    }

    #[test]
    fn monorepo_entry_does_not_attach_root_convention_tests() {
        let s = seed("packages/api/src/index.ts", Language::TypeScript);
        let all = vec![
            "packages/api/src/index.ts".to_string(),
            "tests/index.test.ts".to_string(),
        ];
        let t = nearby_tests(&s, &all);
        assert!(
            !t.contains(&"tests/index.test.ts".to_string()),
            "root tests/ must not attach to a packages/* entry: {t:?}"
        );
    }

    #[test]
    fn identical_stem_does_not_cross_monorepo_packages() {
        let s = seed("packages/api/src/index.ts", Language::TypeScript);
        let all = vec![
            "packages/api/src/index.ts".to_string(),
            "packages/ui/src/index.test.ts".to_string(),
        ];
        let t = nearby_tests(&s, &all);
        assert!(
            !t.contains(&"packages/ui/src/index.test.ts".to_string()),
            "cross-package stem match should not attach: {t:?}"
        );
    }

    #[test]
    fn crates_member_does_not_cross_attach_sibling_crate_test() {
        // Regression: the `crates/` branch of monorepo_member_root was dead, so
        // a crates-workspace source resolved no member root and a same-stem test
        // in a *sibling* crate cross-attached. `crates/foo` must not pull in
        // `crates/bar`'s test.
        let s = seed("crates/foo/src/lib.rs", Language::Rust);
        let all = vec![
            "crates/foo/src/lib.rs".to_string(),
            "crates/bar/tests/lib.rs".to_string(),
        ];
        let t = nearby_tests(&s, &all);
        assert!(
            !t.contains(&"crates/bar/tests/lib.rs".to_string()),
            "sibling-crate test must not cross-attach: {t:?}"
        );
    }

    #[test]
    fn crates_member_attaches_own_crate_test() {
        // The flip side: with the branch live, a crate's own in-tree test still
        // attaches (proves the crates/ branch is no longer dead).
        let s = seed("crates/foo/src/lib.rs", Language::Rust);
        let all = vec![
            "crates/foo/src/lib.rs".to_string(),
            "crates/foo/tests/lib.rs".to_string(),
        ];
        let t = nearby_tests(&s, &all);
        assert!(
            t.contains(&"crates/foo/tests/lib.rs".to_string()),
            "own-crate test should attach: {t:?}"
        );
    }

    #[test]
    fn candidate_partition_preserves_output_on_mixed_fixture() {
        // Locks the optimized (partition + hoisted-format!) path against a
        // fixture that exercises declared prefixes, stem matches, sibling-dir
        // matches, and non-test noise together.
        let mut s = seed("src/app/widget.py", Language::Python);
        s.test_prefixes = vec!["integration".to_string()];
        let all = vec![
            "src/app/widget.py".to_string(),
            "src/app/helper.py".to_string(), // non-test, same dir
            "src/app/test_widget.py".to_string(), // sibling-dir test
            "tests/test_widget.py".to_string(), // convention + stem match
            "tests/test_unrelated.py".to_string(), // convention, no stem
            "integration/widget_smoke_test.py".to_string(), // declared prefix
            "README.md".to_string(),         // wrong language
        ];
        let mut t = nearby_tests(&s, &all);
        t.sort();
        assert_eq!(
            t,
            vec![
                "integration/widget_smoke_test.py".to_string(),
                "src/app/test_widget.py".to_string(),
                "tests/test_widget.py".to_string(),
            ]
        );
    }

    #[test]
    fn c_seed_declared_prefix_attaches_phpt_tests() {
        // php-src extension seeds are C-language but their declared test dirs
        // hold .phpt files. The declared prefix is authoritative: it must
        // attach those tests even though .phpt is not a C extension.
        let mut s = seed("ext/foo/config.m4", Language::C);
        s.test_prefixes = vec!["ext/foo/tests".to_string()];
        let all = vec![
            "ext/foo/config.m4".to_string(),
            "ext/foo/foo.c".to_string(),
            "ext/foo/tests/001.phpt".to_string(),
            "ext/foo/tests/002.phpt".to_string(),
        ];
        let t = nearby_tests(&s, &all);
        assert!(
            t.contains(&"ext/foo/tests/001.phpt".to_string()),
            ".phpt under the declared prefix must attach to a C seed: {t:?}"
        );
        assert!(
            t.contains(&"ext/foo/tests/002.phpt".to_string()),
            ".phpt under the declared prefix must attach to a C seed: {t:?}"
        );
    }

    #[test]
    fn declared_prefix_is_path_anchored() {
        // Prefix "tests" anchors at the repo-relative path start; a nested
        // "other/tests/" dir must not match it.
        let mut s = seed("src/main.rs", Language::Rust);
        s.test_prefixes = vec!["tests".to_string()];
        let all = vec![
            "src/main.rs".to_string(),
            "other/tests/helper_test.py".to_string(),
        ];
        let t = nearby_tests(&s, &all);
        assert!(
            !t.contains(&"other/tests/helper_test.py".to_string()),
            "nested dir must not match a root-anchored declared prefix: {t:?}"
        );
    }

    #[test]
    fn declared_prefix_skips_non_test_shaped_files() {
        // Files under the declared prefix still need to look like a test in
        // SOME language — docs and fixtures data must not attach.
        let mut s = seed("ext/foo/config.m4", Language::C);
        s.test_prefixes = vec!["ext/foo/tests".to_string()];
        let all = vec![
            "ext/foo/tests/README.md".to_string(),
            "ext/foo/tests/001.phpt".to_string(),
        ];
        let t = nearby_tests(&s, &all);
        assert!(
            !t.contains(&"ext/foo/tests/README.md".to_string()),
            "non-code file under declared prefix must not attach: {t:?}"
        );
        assert!(
            t.contains(&"ext/foo/tests/001.phpt".to_string()),
            "test-shaped file under declared prefix must attach: {t:?}"
        );
    }

    #[test]
    fn declared_prefix_prioritizes_seed_language_tests() {
        // Starvation regression: the declared tests/ dir holds five
        // alphabetically-earlier Python test files plus the Rust seed's own
        // integration test. Pre-fix, the prefix loop filled the 5-slot cap
        // in inventory order, so the .py files exhausted it and the one
        // test the seed actually runs never attached. Own-language tests
        // now take the cap slots first.
        let mut s = seed("src/main.rs", Language::Rust);
        s.test_prefixes = vec!["tests".to_string()];
        // Sorted, matching walk_files' production output: the .py files
        // precede the .rs integration test.
        let mut all = vec!["src/main.rs".to_string()];
        for i in 0..5 {
            all.push(format!("tests/test_a{i}.py"));
        }
        all.push("tests/zz_integration.rs".to_string());
        all.sort();
        let t = nearby_tests(&s, &all);
        assert!(
            t.contains(&"tests/zz_integration.rs".to_string()),
            "own-language test must not be starved by other-language files: {t:?}"
        );
        assert!(t.len() <= 5);
    }

    #[test]
    fn declared_test_prefix_not_starved_by_convention_matches() {
        // Five sibling convention tests (same dir as the entry → rule-1 match)
        // would fill the cap; a declared `test_prefixes` entry must still land.
        let mut s = seed("src/widget.py", Language::Python);
        s.test_prefixes = vec!["integration".to_string()];
        let mut all = vec!["src/widget.py".to_string()];
        for i in 0..5 {
            all.push(format!("src/widget_{i}_test.py"));
        }
        all.push("integration/widget_smoke_test.py".to_string());
        let t = nearby_tests(&s, &all);
        assert!(
            t.contains(&"integration/widget_smoke_test.py".to_string()),
            "declared test_prefix must not be starved by convention matches: {t:?}"
        );
    }
}
