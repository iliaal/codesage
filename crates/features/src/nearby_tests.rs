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

pub fn nearby_tests(seed: &FeatureSeed, all_files: &[String]) -> Vec<String> {
    let stem = file_stem(&seed.entry_path);
    let dir = parent_dir(&seed.entry_path);
    let mut out: Vec<String> = Vec::new();
    for f in all_files {
        if f == &seed.entry_path {
            continue;
        }
        if !matches_language(f, seed.language) {
            continue;
        }
        if !is_test_file(f, seed.language) {
            continue;
        }
        if file_relates_to(seed, stem, &dir, f) {
            out.push(f.clone());
            if out.len() >= MAX_TESTS_PER_SEED {
                break;
            }
        }
    }
    // Also accept arbitrary tests inside `seed.test_prefixes`.
    for prefix in &seed.test_prefixes {
        let pfx = prefix.trim_end_matches('/');
        if pfx.is_empty() {
            continue;
        }
        for f in all_files {
            if out.len() >= MAX_TESTS_PER_SEED {
                break;
            }
            if out.iter().any(|x| x == f) {
                continue;
            }
            if !matches_language(f, seed.language) {
                continue;
            }
            if !is_test_file(f, seed.language) {
                continue;
            }
            if f.starts_with(&format!("{pfx}/")) || f == pfx {
                out.push(f.clone());
            }
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
    // 2. Test file whose stem starts with the entry's stem.
    let cand_stem = file_stem(candidate);
    if !stem.is_empty() && (cand_stem.starts_with(stem) || cand_stem.ends_with(stem)) {
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
        if !candidate.starts_with(&format!("{default}/")) {
            continue;
        }
        if always_attach_in_convention_dir {
            return true;
        }
        if stem_match(candidate, stem) {
            return true;
        }
    }
    false
}

fn stem_match(candidate: &str, stem: &str) -> bool {
    if stem.is_empty() {
        return false;
    }
    let cand_stem = file_stem(candidate);
    cand_stem.contains(stem)
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

fn is_test_file(path: &str, language: Language) -> bool {
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
            let base = path.rsplit('/').next().unwrap_or(path);
            base.starts_with("test_")
                || base.ends_with("_test.py")
                || path.starts_with("tests/")
                || path.contains("/tests/")
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
                || path.ends_with(".spec.ts")
                || path.ends_with(".spec.tsx")
                || path.ends_with(".spec.js")
                || path.ends_with(".spec.jsx")
                || path.starts_with("__tests__/")
                || path.contains("/__tests__/")
                || path.starts_with("tests/")
        }
        Language::C | Language::Cpp => {
            path.starts_with("tests/")
                || path.contains("/tests/")
                || path.contains("/test/")
                || path.ends_with("_test.c")
                || path.ends_with("_test.cpp")
                || path.ends_with("_test.cc")
        }
    }
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
    use codesage_protocol::{FeatureConfidence, FeatureKind};

    fn seed(entry: &str, language: Language) -> FeatureSeed {
        FeatureSeed {
            title: String::new(),
            summary: String::new(),
            kind: FeatureKind::CliCommand,
            source: "test",
            confidence: FeatureConfidence::Medium,
            entry_path: entry.to_string(),
            entry_symbol: None,
            entry_route: None,
            entry_command: None,
            test_command: None,
            language,
            tags: Vec::new(),
            owned_files: Vec::new(),
            context_files: Vec::new(),
            tests: Vec::new(),
            test_prefixes: Vec::new(),
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
}
