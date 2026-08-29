//! Sibling-test heuristics per language convention + `recommend_tests` entry
//! point. `risk::assess_risk` also consumes `test_sibling_exists` through the
//! `pub(super)` door.

use anyhow::Result;
use codesage_protocol::{CoupledTestEntry, FileCategory, TestRecommendations};
use codesage_storage::Database;

/// All sibling test files for `file_path` that exist in the index, by language
/// convention, plus a flag for tests that exist but were withheld from the
/// list (the >[`PHPT_LIST_CAP`] `.phpt` case). Used by `recommend_tests` and
/// by `test_sibling_exists`, which must count withheld tests as existing —
/// "too many tests to list" and "no tests" are opposite claims.
fn test_sibling_paths(db: &Database, file_path: &str) -> Result<(Vec<String>, bool)> {
    let mut suppressed = false;
    let stem = file_path
        .rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.'))
        .map(|(s, _)| s.to_string())
        .unwrap_or_default();
    if stem.is_empty() {
        return Ok((Vec::new(), false));
    }
    let dir = file_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");

    let candidates: Vec<String> = vec![
        // PHP: FooTest.php in same dir or app/tests
        format!("{dir}/{stem}Test.php"),
        format!("tests/Unit/{stem}Test.php"),
        format!("tests/Feature/{stem}Test.php"),
        // Python: test_foo.py / foo_test.py in same dir or tests/
        format!("{dir}/test_{stem}.py"),
        format!("{dir}/{stem}_test.py"),
        format!("tests/test_{stem}.py"),
        // Go: foo_test.go
        format!("{dir}/{stem}_test.go"),
        // JS/TS: foo.test.ts(x), foo.spec.ts(x)
        format!("{dir}/{stem}.test.ts"),
        format!("{dir}/{stem}.test.tsx"),
        format!("{dir}/{stem}.test.js"),
        format!("{dir}/{stem}.spec.ts"),
        format!("{dir}/{stem}.spec.tsx"),
        format!("{dir}/{stem}.spec.js"),
        // Rust: foo.rs uses inline #[cfg(test)] mod tests so often no separate
        // file. Skip the explicit rust check; absence here just means the rust
        // file relies on inline tests.
    ];

    let mut found = Vec::new();
    for c in &candidates {
        let normalized = c.trim_start_matches('/').to_string();
        if db.file_id_for_path(&normalized)?.is_some() {
            found.push(normalized);
        }
    }

    // Rust: integration tests live in `<crate_root>/tests/*.rs`, not as siblings
    // and not name-keyed to the source file. List every `.rs` file under the
    // crate's `tests/` directory; the agent can filter further if it has more
    // context. Skips fixture files since those aren't test entry points.
    if file_path.ends_with(".rs")
        && let Some(idx) = file_path.rfind("/src/")
    {
        let crate_root = &file_path[..idx];
        let tests_prefix = format!("{crate_root}/tests/");
        for path in db.indexed_files_with_prefix(&tests_prefix)? {
            if path.ends_with(".rs") && !path.contains("/fixtures/") && !found.contains(&path) {
                found.push(path);
            }
        }
    }
    // Workspace-root case: src/foo.rs paired with tests/*.rs at the same level.
    // Guard against a nested `.../src/.../src/...` path that the crate-local
    // block above already resolved, so we don't list the root `tests/` for a
    // sub-crate source file too.
    if file_path.ends_with(".rs") && file_path.starts_with("src/") && !file_path.contains("/src/") {
        for path in db.indexed_files_with_prefix("tests/")? {
            if path.ends_with(".rs") && !path.contains("/fixtures/") && !found.contains(&path) {
                found.push(path);
            }
        }
    }

    // PHP internals (.c/.h source): .phpt tests live in `<dir>/tests/*.phpt`.
    // The naming convention is loose (bug12345.phpt, gh21709.phpt, feature
    // descriptions) so we list the directory like Rust integration tests
    // rather than try to name-match. The agent or coupled-test signal can
    // narrow further. Skip if the tests dir would dump >PHPT_LIST_CAP files
    // (typical for ext/standard/tests) — too noisy as a "primary"
    // recommendation — but report the suppression so callers don't read the
    // omission as "this file has no tests".
    if (file_path.ends_with(".c") || file_path.ends_with(".h"))
        && let Some((dir, _)) = file_path.rsplit_once('/')
    {
        let tests_prefix = format!("{dir}/tests/");
        let candidates: Vec<String> = db
            .indexed_files_with_prefix(&tests_prefix)?
            .into_iter()
            .filter(|p| p.ends_with(".phpt") && !found.contains(p))
            .collect();
        if candidates.len() <= PHPT_LIST_CAP {
            found.extend(candidates);
        } else {
            suppressed = true;
        }
    }

    // Laravel mirror-tree: source at `app/<rest>/<file>.php` pairs with test at
    // `tests/{Unit,Feature,Integration,Browser}/<rest>/<file>Test.php`. This is
    // the convention most modern Laravel projects use; the flat
    // `tests/Unit/FooTest.php` candidates above only cover root-level sources.
    if file_path.ends_with(".php")
        && let Some(rest) = file_path.strip_prefix("app/")
        && let Some((rest_dir, stem_with_ext)) = rest.rsplit_once('/')
        && let Some((mirror_stem, _)) = stem_with_ext.rsplit_once('.')
    {
        for type_dir in ["Unit", "Feature", "Integration", "Browser"] {
            let candidate = format!("tests/{type_dir}/{rest_dir}/{mirror_stem}Test.php");
            if !found.contains(&candidate) && db.file_id_for_path(&candidate)?.is_some() {
                found.push(candidate);
            }
        }
    }

    // Symfony mirror-tree: source at `src/<rest>/<file>.php` pairs with test at
    // `tests/<rest>/<file>Test.php` (no Unit/Feature subdivisor; Symfony tests
    // mirror src/ directly).
    if file_path.ends_with(".php")
        && let Some(rest) = file_path.strip_prefix("src/")
        && let Some((rest_dir, stem_with_ext)) = rest.rsplit_once('/')
        && let Some((mirror_stem, _)) = stem_with_ext.rsplit_once('.')
    {
        let candidate = format!("tests/{rest_dir}/{mirror_stem}Test.php");
        if !found.contains(&candidate) && db.file_id_for_path(&candidate)?.is_some() {
            found.push(candidate);
        }
    }

    Ok((found, suppressed))
}

/// Largest `.phpt` directory listing surfaced verbatim in `primary`. Above
/// this the directory is named in a note instead of dumped file-by-file.
const PHPT_LIST_CAP: usize = 50;

/// Heuristic: do any indexed files look like tests for `file_path`? Exposed as
/// `pub(super)` so `risk::assess_risk` can consume it without re-implementing
/// sibling detection. A `.phpt` directory withheld from listing by
/// [`PHPT_LIST_CAP`] still counts: those tests exist.
pub(super) fn test_sibling_exists(db: &Database, file_path: &str) -> Result<bool> {
    let (paths, suppressed) = test_sibling_paths(db, file_path)?;
    Ok(!paths.is_empty() || suppressed)
}

/// Tests an agent should run after editing the given files. Two layers:
/// sibling tests (high confidence, language convention) plus tests that
/// historically co-change (medium confidence, catches integration-style
/// tests that don't follow naming conventions). Empty result means no
/// matching test files in the index.
pub fn recommend_tests(db: &Database, file_paths: &[String]) -> Result<TestRecommendations> {
    use std::collections::HashSet;

    let mut primary: HashSet<String> = HashSet::new();
    let mut coupled: Vec<CoupledTestEntry> = Vec::new();
    let mut suppressed_sources: Vec<String> = Vec::new();

    for path in file_paths {
        let (siblings, suppressed) = test_sibling_paths(db, path)?;
        if suppressed {
            suppressed_sources.push(path.clone());
        }
        for sibling in siblings {
            primary.insert(sibling);
        }
        let co = db.co_changes_for(path, 20)?;
        for entry in co {
            if matches!(FileCategory::classify(&entry.file), FileCategory::Test) {
                coupled.push(CoupledTestEntry {
                    file: entry.file,
                    weight: entry.weight,
                    count: entry.count,
                    source: path.clone(),
                });
            }
        }
    }

    // Drop coupled entries that are also in primary; primary already says "run me".
    coupled.retain(|c| !primary.contains(&c.file));

    // Dedupe coupled entries by file, keeping the highest-weight pairing so the
    // agent sees the strongest signal. Source attribution refers to that pairing.
    coupled.sort_by(|a, b| {
        b.weight
            .partial_cmp(&a.weight)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut seen: HashSet<String> = HashSet::new();
    coupled.retain(|e| seen.insert(e.file.clone()));

    let mut primary_sorted: Vec<String> = primary.into_iter().collect();
    primary_sorted.sort();

    let mut notes = Vec::new();
    if primary_sorted.is_empty() && coupled.is_empty() && suppressed_sources.is_empty() {
        notes.push(
            "no test files found via sibling conventions or co-change history; \
             run `codesage git-index` if you haven't, or add tests for these files"
                .to_string(),
        );
    } else {
        if !primary_sorted.is_empty() {
            notes.push(format!(
                "{} sibling test file(s) found by language convention",
                primary_sorted.len()
            ));
        }
        if !coupled.is_empty() {
            notes.push(format!(
                "{} additional test file(s) suggested by co-change history",
                coupled.len()
            ));
        }
        if !suppressed_sources.is_empty() {
            // Withheld, not absent: without this note the cap would turn a
            // heavily-tested extension file into an "untested" report.
            notes.push(format!(
                "tests/ directory next to {} holds more than {PHPT_LIST_CAP} .phpt files — \
                 omitted from `primary` to keep output bounded; run that directory's suite",
                suppressed_sources.join(", ")
            ));
        }
    }

    Ok(TestRecommendations {
        primary: primary_sorted,
        coupled,
        notes,
    })
}
