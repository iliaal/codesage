//! Per-edit context from committed history and test naming conventions.
//! Avoid full risk analysis on this latency-sensitive path and test-gap claims
//! with poor precision. Staleness annotates rather than suppresses history.

use anyhow::Result;
use codesage_protocol::EditBrief;
use codesage_storage::Database;

use crate::git_history::{find_coupling, recommend_tests};

const MAX_COUPLED: usize = 5;

const MAX_TESTS: usize = 5;

const CHURN_NOTABLE: f64 = 0.75;

// Percentiles can rank a one-commit file highly in a repository with little history.
const MIN_COMMITS_FOR_CHURN: u32 = 5;

/// Callers serving unrequested context must emit nothing when `empty` is set.
pub fn build_edit_brief(
    db: &Database,
    file_path: &str,
    on_disk_hash: Option<&str>,
) -> Result<EditBrief> {
    let mut brief = EditBrief {
        file_path: file_path.to_string(),
        ..Default::default()
    };

    if let Some(row) = db.git_file(file_path)? {
        brief.fix_count = Some(row.fix_count);
        brief.commits = Some(row.total_commits);
        brief.churn_percentile = Some(db.churn_percentile(file_path)?);
    }

    let coupling = find_coupling(db, file_path, MAX_COUPLED)?;
    brief.coupled = coupling.coupled.into_iter().map(|c| c.file).collect();

    let recs = recommend_tests(db, std::slice::from_ref(&file_path.to_string()))?;
    // Crate-wide tests do not identify this source's tests. Co-change alone
    // belongs under `coupled`, since it does not establish test coverage.
    brief.tests = recs
        .primary
        .into_iter()
        .filter(|t| test_names_source(t, file_path))
        .take(MAX_TESTS)
        .collect();

    if let Some(disk) = on_disk_hash
        && let Some(indexed) = db.get_file_hash(file_path)?
    {
        brief.stale = disk != indexed;
    }

    brief.hotspot = brief.churn_percentile.unwrap_or(0.0) >= CHURN_NOTABLE
        && brief.commits.unwrap_or(0) >= MIN_COMMITS_FOR_CHURN;
    brief.empty = brief.tests.is_empty() && brief.coupled.is_empty() && !brief.hotspot;
    Ok(brief)
}

fn test_names_source(test_path: &str, source_path: &str) -> bool {
    let stem = |p: &str| {
        p.rsplit('/')
            .next()
            .unwrap_or(p)
            .split('.')
            .next()
            .unwrap_or("")
            .to_lowercase()
    };
    let src = stem(source_path);
    // Short stems such as `db` are too common to identify a source reliably.
    if src.chars().count() < 3 {
        return false;
    }
    let test_stem = stem(test_path);
    if test_stem == src {
        return true;
    }
    // Substring matching would let `user` claim `test_superuser_auth.py`.
    strip_one_test_affix(&test_stem).is_some_and(|stripped| stripped == src)
}

fn strip_one_test_affix(stem: &str) -> Option<&str> {
    for prefix in ["test_", "test-"] {
        if let Some(rest) = stem.strip_prefix(prefix) {
            return Some(rest);
        }
    }
    for suffix in ["_test", "-test", "_spec", "-spec", "tests", "test", "spec"] {
        if let Some(rest) = stem.strip_suffix(suffix) {
            return Some(rest);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::test_names_source;

    #[test]
    fn only_tests_named_after_the_source_count_as_siblings() {
        for (test, src) in [
            ("tests/RepositoryTest.php", "app/Repository.php"),
            ("src/foo.test.ts", "src/foo.ts"),
            ("tests/test_parser.py", "app/parser.py"),
            ("pkg/server_test.go", "pkg/server.go"),
        ] {
            assert!(test_names_source(test, src), "{test} should match {src}");
        }

        for test in [
            "crates/graph/tests/impact_test.rs",
            "crates/graph/tests/risk_test.rs",
        ] {
            assert!(
                !test_names_source(test, "crates/graph/src/search.rs"),
                "{test} must not read as a sibling of search.rs"
            );
        }

        assert!(!test_names_source("tests/db_helper_test.rs", "src/db.rs"));

        assert!(!test_names_source(
            "tests/test_superuser_auth.py",
            "app/user.py"
        ));
        assert!(test_names_source("tests/user_test.py", "app/user.py"));
    }
}
