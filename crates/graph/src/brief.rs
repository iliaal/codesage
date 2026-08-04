//! The payload a serve-side caller can afford to assemble on every edit.
//!
//! Built for `cs-sweep-serve-dont-recommend-2l0`: two independent measurements
//! found agents ignore retrieval *recommendations* (0.2% action rate across 502
//! firings in one production replay), so the useful move is to serve a small
//! answer rather than ask for a tool call. That only works if assembling it is
//! nearly free and if saying nothing is the common case.
//!
//! What that rules out is most of the interesting data. `assess_risk` measured
//! 464-675ms on real indexes because it walks import cycles and per-symbol
//! reference counts, so nothing here calls it. `test_gap` is excluded on
//! precision: its false-positive rate measured 15-25%.
//!
//! What survives is history- and convention-derived, which has a second
//! property worth more than its cost: an unsaved edit cannot invalidate it.
//! Churn, fix counts and co-change come from committed history; sibling tests
//! come from naming conventions. That is why a stale index only annotates this
//! payload instead of suppressing it.

use anyhow::Result;
use codesage_protocol::EditBrief;
use codesage_storage::Database;

use crate::git_history::{find_coupling, recommend_tests};

/// Co-changed files carried into the brief. Past a handful this stops being a
/// hint and starts being a list the reader skims, and it competes for context
/// with whatever the agent was actually doing.
const MAX_COUPLED: usize = 5;

/// Tests named in the brief, for the same reason.
const MAX_TESTS: usize = 5;

/// Churn percentile below which a file is not worth mentioning on its own.
/// A file nothing else recommends and that is not a hotspot produces silence.
const CHURN_NOTABLE: f64 = 0.75;

/// Commits a file needs before its churn rank is allowed to mean anything.
///
/// Churn percentile ranks a file against the rest of the repo, so the top
/// quartile is occupied no matter how little has happened: on two real repos
/// files with one and two commits ranked at the 90th percentile and above. The
/// line this feeds reports a fix *ratio*, which carries no information over a
/// handful of samples, and a false hotspot is exactly the noise that teaches an
/// agent to stop reading an unrequested channel.
const MIN_COMMITS_FOR_CHURN: u32 = 5;

/// Assemble the brief for `file_path` in one pass.
///
/// Returns a brief with `empty` set when there is nothing worth saying, which
/// is the expected outcome for most files. Callers serving this unasked must
/// emit nothing at all in that case rather than a "no findings" line.
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
    // Only sibling-convention matches, and only when the test is NAMED AFTER
    // this file. Two separate reasons, both measured on real payloads:
    //
    // Rust's convention resolves to every integration test under
    // `<crate>/tests/*.rs`, so it returns the same seven files for every source
    // file in the crate. Serving that per edit would make `empty` unreachable in
    // a Rust crate.
    //
    // `recs.coupled` is excluded outright. A co-changed test is a correlation,
    // not a test OF this file, and the word "tests" claims the second thing.
    // Reading the 67 distinct payloads this produces across three repos turned
    // up `README.md -> tests: test_review_state.py`, `Cargo.toml ->
    // tests: risk_test.rs`, `regression-tests.sh -> tests: impact_test.rs`.
    // Nothing is lost by dropping them: a test that genuinely co-changes is
    // already eligible for `coupled` below, which is the honest label for it.
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

/// True when `test_path`'s file name is derived from `source_path`'s, the
/// relationship every sibling convention encodes: `Repository.php` ->
/// `RepositoryTest.php`, `foo.ts` -> `foo.test.ts`, `foo.py` -> `test_foo.py`,
/// `foo.go` -> `foo_test.go`.
///
/// A three-character floor keeps a short stem like `db` from matching most of
/// the tree.
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
    if src.chars().count() < 3 {
        return false;
    }
    let name = test_path
        .rsplit('/')
        .next()
        .unwrap_or(test_path)
        .to_lowercase();
    name.contains(&src)
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

        // Rust integration tests are crate-level: the same list comes back for
        // every source file in the crate, so none of them name their source.
        for test in [
            "crates/graph/tests/impact_test.rs",
            "crates/graph/tests/risk_test.rs",
        ] {
            assert!(
                !test_names_source(test, "crates/graph/src/search.rs"),
                "{test} must not read as a sibling of search.rs"
            );
        }

        // A stem too short to discriminate matches nothing.
        assert!(!test_names_source("tests/db_helper_test.rs", "src/db.rs"));
    }
}
