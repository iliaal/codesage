//! Git history indexer: `git log` pass, decay math, txn-wrapped writes.
//!
//! Source patterns from repowise's git_indexer.py, re-implemented from algorithm:
//! - one subprocess for the whole repo
//! - exponential decay (τ=180 days) on commit age
//! - per-commit churn weight = decay * min((added+deleted)/100, 3.0); the clamp
//!   prevents one historic refactor from dominating forever
//! - co-change pair weight = sum over commits where both files appear, weighted by decay
//! - min co-change count = 3 (drop pairs that only ever changed together once or twice)
//! - soft-skip `chore:` / `build:` commits UNLESS message contains migrate/refactor/adopt/deprecate
//! - no-merges only (merge commits double-count work already in their parents)

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, anyhow};
use codesage_parser::discover::{
    DEFAULT_EXCLUDE_PATTERNS, TEST_LIKE_EXCLUDE_PATTERNS, build_exclude_set,
};
use codesage_protocol::GitIndexStats;
use codesage_storage::Database;
use globset::GlobSet;

const DECAY_HALFLIFE_DAYS: f64 = 180.0;
const SECONDS_PER_DAY: f64 = 86_400.0;
const CHURN_CLAMP: f64 = 3.0;
const CHURN_DIVISOR: f64 = 100.0;
const MIN_CO_CHANGE_COUNT: u32 = 3;
/// Avoid building O(n²) pair sets for sweeping refactor commits. Anything bigger than this
/// is almost certainly a vendored update or auto-formatter, not a meaningful co-change.
const MAX_FILES_PER_COMMIT_FOR_COCHANGE: usize = 30;

#[derive(Debug, Default)]
struct FileStats {
    churn_score: f64,
    fix_count: u32,
    total_commits: u32,
    last_commit_at: Option<i64>,
}

#[derive(Debug, Default)]
struct PairStats {
    weight: f64,
    count: u32,
    last_observed_at: Option<i64>,
}

/// Indexing mode. `Auto` is the recommended default — reuses prior state if valid,
/// falls back to full rescan otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexMode {
    /// Re-scan the whole history. Drops existing git_files/git_co_changes first.
    Full,
    /// Only scan commits after the last recorded SHA. Falls back to Full if state
    /// is missing, corrupted, or the prior SHA isn't an ancestor of HEAD.
    Incremental,
    /// Incremental if state is valid, else full. Default for hooks and CLI.
    Auto,
}

/// Shorthand: full-mode scan with no extra excludes. Used by tests that want the
/// simplest entry point; production callers go through `git_history_index_with_options`.
pub fn git_history_index(db: &Database, root: &Path) -> Result<GitIndexStats> {
    git_history_index_with_options(db, root, &[], IndexMode::Full)
}

/// Full control: excludes + mode. Public entry point for CLI/hook callers that want
/// incremental behavior.
pub fn git_history_index_with_options(
    db: &Database,
    root: &Path,
    extra_excludes: &[String],
    mode: IndexMode,
) -> Result<GitIndexStats> {
    let (exclude_set, test_like_set) = compile_excludes(extra_excludes)?;
    let head_sha = resolve_head_sha(root)?;

    let effective_mode = match mode {
        IndexMode::Full => IndexMode::Full,
        IndexMode::Incremental | IndexMode::Auto => match db.get_git_index_state()? {
            Some((last_sha, _)) if last_sha == head_sha => {
                // Already up to date. Refresh indexed_at so decay stays anchored to now.
                db.set_git_index_state(&head_sha)?;
                return Ok(GitIndexStats {
                    commits_scanned: 0,
                    files_tracked: 0,
                    co_change_pairs: 0,
                });
            }
            Some((last_sha, _)) => {
                if is_ancestor(root, &last_sha, &head_sha)? {
                    IndexMode::Incremental
                } else {
                    IndexMode::Full
                }
            }
            None => IndexMode::Full,
        },
    };

    match effective_mode {
        IndexMode::Full => run_full(db, root, &exclude_set, &test_like_set, &head_sha),
        IndexMode::Incremental => {
            let (last_sha, last_at) = db
                .get_git_index_state()?
                .expect("incremental path checked state present above");
            run_incremental(
                db,
                root,
                &exclude_set,
                &test_like_set,
                &head_sha,
                &last_sha,
                last_at,
            )
        }
        IndexMode::Auto => unreachable!("resolved above"),
    }
}

/// Returns two glob sets:
/// - `hard_exclude`: files that don't enter `git_files` at all (vendor, build
///   outputs, binaries, lock files, generated docs).
/// - `test_like`: files that enter `git_files` (so `recommend_tests` and
///   `assess_risk` test-gap detection can find them) but are dropped from
///   co-change pair generation. Tests, benches.
fn compile_excludes(extra: &[String]) -> Result<(GlobSet, GlobSet)> {
    use std::collections::HashSet;

    let test_like_set: HashSet<&&str> = TEST_LIKE_EXCLUDE_PATTERNS.iter().collect();

    let mut hard: Vec<String> = DEFAULT_EXCLUDE_PATTERNS
        .iter()
        .filter(|p| !test_like_set.contains(p))
        .map(|s| s.to_string())
        .collect();
    hard.extend(extra.iter().cloned());
    let hard_set =
        build_exclude_set(&hard).with_context(|| "compiling git history hard-exclude patterns")?;

    let test_patterns: Vec<String> = TEST_LIKE_EXCLUDE_PATTERNS
        .iter()
        .map(|s| s.to_string())
        .collect();
    let test_set = build_exclude_set(&test_patterns)
        .with_context(|| "compiling test-like exclude patterns")?;

    Ok((hard_set, test_set))
}

fn run_full(
    db: &Database,
    root: &Path,
    exclude_set: &GlobSet,
    test_like_set: &GlobSet,
    head_sha: &str,
) -> Result<GitIndexStats> {
    let now = unix_now();
    let raw = run_git_log(root, None)?;
    let commits = parse_log(&raw);

    let mut files: HashMap<String, FileStats> = HashMap::new();
    let mut pairs: HashMap<(String, String), PairStats> = HashMap::new();
    let mut commits_scanned = 0usize;

    for commit in &commits {
        let Some(kept_changes) = filter_kept(commit, exclude_set) else {
            continue;
        };
        commits_scanned += 1;
        accumulate(
            &mut files,
            &mut pairs,
            commit,
            &kept_changes,
            now,
            test_like_set,
        );
    }

    // Wrap clear + every upsert in one transaction. SQLite default-mode commits
    // each execute, which means N rows = N fsync()s. On large repos (php-src
    // ~25k files + ~10k pairs) that turns into a multi-minute disk wait
    // (jbd2_log_wait_commit). One transaction = one fsync.
    let mut co_change_kept = 0usize;
    db.execute_batch(|db| {
        db.clear_git_data()?;
        for (path, stats) in &files {
            db.upsert_git_file(
                path,
                stats.churn_score,
                stats.fix_count,
                stats.total_commits,
                stats.last_commit_at,
            )?;
        }
        for ((a, b), stats) in &pairs {
            if stats.count >= MIN_CO_CHANGE_COUNT {
                db.upsert_git_co_change(a, b, stats.weight, stats.count, stats.last_observed_at)?;
                co_change_kept += 1;
            }
        }
        db.set_git_index_state(head_sha)?;
        Ok(())
    })?;

    Ok(GitIndexStats {
        commits_scanned,
        files_tracked: files.len(),
        co_change_pairs: co_change_kept,
    })
}

fn run_incremental(
    db: &Database,
    root: &Path,
    exclude_set: &GlobSet,
    test_like_set: &GlobSet,
    head_sha: &str,
    last_sha: &str,
    last_indexed_at: i64,
) -> Result<GitIndexStats> {
    let now = unix_now();
    let range = format!("{last_sha}..{head_sha}");
    let raw = run_git_log(root, Some(&range))?;
    let commits = parse_log(&raw);

    let mut files: HashMap<String, FileStats> = HashMap::new();
    let mut pairs: HashMap<(String, String), PairStats> = HashMap::new();
    let mut commits_scanned = 0usize;

    for commit in &commits {
        let Some(kept_changes) = filter_kept(commit, exclude_set) else {
            continue;
        };
        commits_scanned += 1;
        accumulate(
            &mut files,
            &mut pairs,
            commit,
            &kept_changes,
            now,
            test_like_set,
        );
    }

    // Preload existing pair keys once, outside the write transaction, so the
    // inner loop's "does this sub-threshold pair already exist in DB?" check is
    // an in-memory HashMap lookup instead of one `SELECT COUNT(*)` per pair.
    let existing_pairs = db.all_co_change_pairs()?;

    // One transaction wraps decay-scale + every upsert. See run_full for the
    // fsync motivation. Decay scale needs to be inside the same transaction
    // as the deltas, otherwise a crash mid-write leaves us with scaled-but-
    // not-incremented rows.
    let mut co_change_kept = 0usize;
    db.execute_batch(|db| {
        let delta_seconds = (now - last_indexed_at).max(0) as f64;
        if delta_seconds > 0.0 {
            let factor = (-delta_seconds / (DECAY_HALFLIFE_DAYS * SECONDS_PER_DAY)).exp();
            db.scale_git_decay(factor)?;
        }
        for (path, stats) in &files {
            db.incr_git_file(
                path,
                stats.churn_score,
                stats.fix_count,
                stats.total_commits,
                stats.last_commit_at,
            )?;
        }
        for ((a, b), stats) in &pairs {
            // Surface a pair if either it already exists in DB (accumulate onto it) or
            // its delta alone cleared the min-count filter. Sub-threshold pairs that
            // straddle the boundary will be caught by the next full rescan.
            let pair_exists = existing_pairs.get(a).is_some_and(|rhs| rhs.contains(b));
            if stats.count >= MIN_CO_CHANGE_COUNT || pair_exists {
                db.incr_git_co_change(a, b, stats.weight, stats.count, stats.last_observed_at)?;
                co_change_kept += 1;
            }
        }
        db.set_git_index_state(head_sha)?;
        Ok(())
    })?;

    Ok(GitIndexStats {
        commits_scanned,
        files_tracked: files.len(),
        co_change_pairs: co_change_kept,
    })
}

fn filter_kept<'a>(commit: &'a Commit, exclude_set: &GlobSet) -> Option<Vec<&'a FileChange>> {
    if soft_skip(&commit.subject) {
        return None;
    }
    let kept: Vec<&FileChange> = commit
        .changes
        .iter()
        .filter(|c| !is_excluded(exclude_set, &c.path))
        .collect();
    if kept.is_empty() { None } else { Some(kept) }
}

fn accumulate(
    files: &mut HashMap<String, FileStats>,
    pairs: &mut HashMap<(String, String), PairStats>,
    commit: &Commit,
    kept_changes: &[&FileChange],
    now: i64,
    test_like_set: &GlobSet,
) {
    let age_days = ((now - commit.timestamp).max(0) as f64) / SECONDS_PER_DAY;
    let decay = (-age_days / DECAY_HALFLIFE_DAYS).exp();
    let is_fix = is_fix_commit(&commit.subject);

    for change in kept_changes {
        let stats = files.entry(change.path.clone()).or_default();
        stats.total_commits += 1;
        if is_fix {
            stats.fix_count += 1;
        }
        if stats.last_commit_at.is_none_or(|t| t < commit.timestamp) {
            stats.last_commit_at = Some(commit.timestamp);
        }
        let churn_units = ((change.added + change.deleted) as f64) / CHURN_DIVISOR;
        let weight = decay * churn_units.min(CHURN_CLAMP);
        stats.churn_score += weight;
    }

    // Skip pairs where BOTH sides are test-like. Test-test co-changes are noise
    // (running multiple test files in one PR doesn't imply the underlying code
    // is related). Source-test pairs are kept — that's the signal
    // `recommend_tests` uses to surface tests that historically follow a source
    // change (essential for codebases like php-src where .phpt tests are the
    // primary partner of .c source edits). Source-source pairs are kept as before.
    if kept_changes.len() <= MAX_FILES_PER_COMMIT_FOR_COCHANGE {
        for i in 0..kept_changes.len() {
            for j in (i + 1)..kept_changes.len() {
                let a = &kept_changes[i].path;
                let b = &kept_changes[j].path;
                if test_like_set.is_match(a) && test_like_set.is_match(b) {
                    continue;
                }
                let (lo, hi) = if a < b { (a, b) } else { (b, a) };
                let pair = pairs.entry((lo.clone(), hi.clone())).or_default();
                pair.weight += decay;
                pair.count += 1;
                if pair.last_observed_at.is_none_or(|t| t < commit.timestamp) {
                    pair.last_observed_at = Some(commit.timestamp);
                }
            }
        }
    }
}

fn resolve_head_sha(root: &Path) -> Result<String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .with_context(|| format!("git rev-parse HEAD in {}", root.display()))?;
    if !out.status.success() {
        return Err(anyhow!(
            "git rev-parse HEAD failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8(out.stdout)
        .context("HEAD SHA not UTF-8")?
        .trim()
        .to_string())
}

fn is_ancestor(root: &Path, old: &str, new: &str) -> Result<bool> {
    // Distinguishes spawn failure (git missing / setup error -> propagate) from a clean
    // exit-1 (genuine "not an ancestor", caller falls back to full rescan). Exit-128
    // (sha doesn't exist after force-push or shallow clone) is also Ok(false): same
    // recovery path, but worth a note in logs because it tells you why a hook is
    // doing extra work.
    let status = Command::new("git")
        .args(["merge-base", "--is-ancestor", old, new])
        .current_dir(root)
        .status()
        .with_context(|| {
            format!(
                "spawning `git merge-base --is-ancestor {old} {new}` in {}",
                root.display()
            )
        })?;
    if !status.success() && status.code() == Some(128) {
        tracing::warn!(
            %old,
            "git rejected old SHA (likely rewritten history); falling back to full rescan"
        );
    }
    Ok(status.success())
}

#[derive(Debug)]
struct Commit {
    timestamp: i64,
    subject: String,
    changes: Vec<FileChange>,
}

#[derive(Debug)]
struct FileChange {
    path: String,
    added: u32,
    deleted: u32,
}

fn run_git_log(root: &Path, range: Option<&str>) -> Result<String> {
    let mut args: Vec<&str> = vec![
        "log",
        "--no-merges",
        "--numstat",
        "--pretty=format:commit\x09%H\x09%ct\x09%s",
    ];
    if let Some(r) = range {
        args.push(r);
    }
    let output = Command::new("git")
        .args(&args)
        .current_dir(root)
        .output()
        .with_context(|| format!("running git log in {}", root.display()))?;

    if !output.status.success() {
        return Err(anyhow!(
            "git log failed in {}: {}",
            root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout).context("git log output not UTF-8")
}

fn parse_log(raw: &str) -> Vec<Commit> {
    let mut commits = Vec::new();
    let mut current: Option<Commit> = None;
    let mut skipped_commits = 0usize;
    let mut skipped_changes = 0usize;

    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("commit\t") {
            if let Some(prev) = current.take() {
                commits.push(prev);
            }
            let mut parts = rest.splitn(3, '\t');
            let _sha = parts.next().unwrap_or("");
            // Drop commits with unparseable timestamps. ts=0 would survive as a
            // 1970-01-01 commit that contributes nothing to churn (decay≈0) yet still
            // increments fix_count and total_commits, silently skewing fix_ratio.
            let ts: Option<i64> = parts.next().and_then(|s| s.parse().ok());
            let Some(ts) = ts else {
                skipped_commits += 1;
                current = None;
                continue;
            };
            let subject = parts.next().unwrap_or("").to_string();
            current = Some(Commit {
                timestamp: ts,
                subject,
                changes: Vec::new(),
            });
            continue;
        }
        if line.is_empty() {
            continue;
        }
        let Some(commit) = current.as_mut() else {
            continue;
        };
        // numstat line: "<added>\t<deleted>\t<path>"; binary files use "-\t-\t<path>"
        let mut parts = line.splitn(3, '\t');
        let added_s = parts.next().unwrap_or("-");
        let deleted_s = parts.next().unwrap_or("-");
        let path = parts.next().unwrap_or("");
        if path.is_empty() || added_s == "-" || deleted_s == "-" {
            continue;
        }
        // Drop the change rather than fabricate zeros: a parse failure was
        // indistinguishable from "0 lines added/deleted", which means a corrupt log
        // line silently turned into a zero-churn commit-to-file entry.
        let (Ok(added), Ok(deleted)) = (added_s.parse::<u32>(), deleted_s.parse::<u32>()) else {
            skipped_changes += 1;
            continue;
        };
        // Rename detection in numstat looks like `path/{old => new}/file`. Normalize
        // to the destination by stripping `{old => ` and `}`.
        let normalized = normalize_rename_path(path);
        commit.changes.push(FileChange {
            path: normalized,
            added,
            deleted,
        });
    }
    if let Some(prev) = current.take() {
        commits.push(prev);
    }
    if skipped_commits > 0 || skipped_changes > 0 {
        tracing::warn!(
            skipped_commits,
            skipped_changes,
            "parse_log skipped unparseable git output entries"
        );
    }
    commits
}

fn normalize_rename_path(raw: &str) -> String {
    // git format examples:
    //   src/{foo.rs => bar.rs}
    //   src/{old => new}/inner/file.rs
    //   {old/dir => new/dir}/file.rs
    if let (Some(open), Some(close)) = (raw.find('{'), raw.find('}'))
        && open < close
        && let Some(arrow) = raw[open..close].find(" => ")
    {
        let prefix = &raw[..open];
        let after_arrow_in_braces = &raw[open + arrow + 4..close];
        let suffix = &raw[close + 1..];
        return format!("{prefix}{after_arrow_in_braces}{suffix}");
    }
    raw.to_string()
}

fn is_fix_commit(subject: &str) -> bool {
    let s = subject.to_ascii_lowercase();
    s.starts_with("fix:")
        || s.starts_with("fix(")
        || s.starts_with("bugfix:")
        || s.starts_with("hotfix:")
        || s.contains(" fix ")
        || s.contains(" fixes ")
        || s.contains("fixes #")
        || s.contains("fixes gh-")
        || s.contains("closes #")
}

fn soft_skip(subject: &str) -> bool {
    let s = subject.to_ascii_lowercase();
    let is_chore = s.starts_with("chore:")
        || s.starts_with("chore(")
        || s.starts_with("build:")
        || s.starts_with("build(")
        || s.starts_with("ci:")
        || s.starts_with("style:")
        || s.starts_with("docs:");
    if !is_chore {
        return false;
    }
    let rescued = s.contains("migrate")
        || s.contains("refactor")
        || s.contains("adopt")
        || s.contains("deprecate")
        || s.contains("upgrade");
    !rescued
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn is_excluded(set: &GlobSet, path: &str) -> bool {
    set.is_match(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_fix_commits() {
        assert!(is_fix_commit("fix: avoid UAF in foo"));
        assert!(is_fix_commit("Fix(parser): off-by-one"));
        assert!(is_fix_commit("Fixes #1234"));
        assert!(is_fix_commit("hotfix: prod outage"));
        assert!(!is_fix_commit("feat: add foo"));
        assert!(!is_fix_commit("refactor parser internals"));
    }

    #[test]
    fn soft_skip_filters_chores_but_rescues_meaningful() {
        assert!(soft_skip("chore: bump deps"));
        assert!(soft_skip("build: switch to bun"));
        assert!(!soft_skip("chore: migrate from yarn to pnpm"));
        assert!(!soft_skip("chore(refactor): rename internal helpers"));
        assert!(!soft_skip("feat: add foo"));
    }

    #[test]
    fn rename_normalization() {
        assert_eq!(
            normalize_rename_path("src/{foo.rs => bar.rs}"),
            "src/bar.rs"
        );
        assert_eq!(
            normalize_rename_path("src/{old => new}/inner.rs"),
            "src/new/inner.rs"
        );
        assert_eq!(
            normalize_rename_path("{old/dir => new/dir}/file.rs"),
            "new/dir/file.rs"
        );
        assert_eq!(normalize_rename_path("plain/path.rs"), "plain/path.rs");
    }

    #[test]
    fn parse_log_handles_basic_format() {
        let raw = "commit\tabc\t1700000000\tfix: x\n10\t2\tsrc/a.rs\n5\t1\tsrc/b.rs\n\
                   commit\tdef\t1700001000\tfeat: y\n3\t0\tsrc/c.rs\n-\t-\tbinary.bin\n";
        let commits = parse_log(raw);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].subject, "fix: x");
        assert_eq!(commits[0].changes.len(), 2);
        assert_eq!(commits[1].changes.len(), 1, "binary file skipped");
        assert_eq!(commits[1].changes[0].path, "src/c.rs");
    }

    fn make_change(path: &str) -> FileChange {
        FileChange {
            path: path.to_string(),
            added: 5,
            deleted: 1,
        }
    }

    fn test_glob() -> GlobSet {
        build_exclude_set(&["**/*Test.php".to_string(), "**/*.phpt".to_string()])
            .expect("build glob set")
    }

    #[test]
    fn accumulate_keeps_source_test_pair_drops_test_test_pair() {
        // The v0.3.1 fix: the previous behavior dropped tests entirely from
        // pair generation; this test pins down the corrected rule.
        let now = 1_700_000_100;
        let commit = Commit {
            timestamp: 1_700_000_000,
            subject: "fix: thing".into(),
            changes: vec![],
        };
        let changes = [
            make_change("Repository.php"),     // source
            make_change("RepositoryTest.php"), // test
            make_change("AnotherTest.php"),    // test
        ];
        let kept: Vec<&FileChange> = changes.iter().collect();
        let mut files = HashMap::new();
        let mut pairs = HashMap::new();
        accumulate(&mut files, &mut pairs, &commit, &kept, now, &test_glob());

        // Source <-> test pair MUST be present (the v0.3.1 signal we want).
        assert!(
            pairs.contains_key(&("Repository.php".into(), "RepositoryTest.php".into())),
            "source-test pair must be kept; got pairs: {:?}",
            pairs.keys().collect::<Vec<_>>()
        );
        assert!(
            pairs.contains_key(&("AnotherTest.php".into(), "Repository.php".into())),
            "source-test pair must be kept regardless of stem"
        );
        // Test <-> test pair MUST NOT be present (still noise).
        assert!(
            !pairs.contains_key(&("AnotherTest.php".into(), "RepositoryTest.php".into())),
            "test-test pair must be skipped"
        );
        // All three files contribute to per-file churn (test files stay in git_files).
        assert!(files.contains_key("Repository.php"));
        assert!(files.contains_key("RepositoryTest.php"));
        assert!(files.contains_key("AnotherTest.php"));
    }

    #[test]
    fn accumulate_keeps_source_source_pairs_when_tests_present() {
        // Sanity: source <-> source pairs unaffected by the test filter.
        let now = 1_700_000_100;
        let commit = Commit {
            timestamp: 1_700_000_000,
            subject: "feat: x".into(),
            changes: vec![],
        };
        let changes = [
            make_change("Repository.php"),
            make_change("Service.php"),
            make_change("RepositoryTest.php"),
        ];
        let kept: Vec<&FileChange> = changes.iter().collect();
        let mut files = HashMap::new();
        let mut pairs = HashMap::new();
        accumulate(&mut files, &mut pairs, &commit, &kept, now, &test_glob());

        assert!(
            pairs.contains_key(&("Repository.php".into(), "Service.php".into())),
            "source-source pair must always be kept"
        );
    }
}
