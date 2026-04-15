//! Git history indexer (V2b slice 1).
//!
//! Populates `git_files` (per-file churn / fix counts) and `git_co_changes`
//! (per-pair historical co-change weight) from a single `git log` pass.
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
use codesage_protocol::{
    CoChangeEntry, CoupledTestEntry, FileCategory, GitIndexStats, ImpactRequest, ImpactTarget,
    RiskAssessment, RiskDiffAssessment, TestRecommendations,
};
use codesage_storage::Database;
use globset::GlobSet;

use crate::query::impact_analysis;

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

pub fn git_history_index(db: &Database, root: &Path) -> Result<GitIndexStats> {
    git_history_index_with_options(db, root, &default_excludes(), IndexMode::Full)
}

/// Same as `git_history_index` but with a caller-provided exclude pattern list.
/// Defaults to Full mode for API compatibility.
pub fn git_history_index_with_excludes(
    db: &Database,
    root: &Path,
    extra_excludes: &[String],
) -> Result<GitIndexStats> {
    git_history_index_with_options(db, root, extra_excludes, IndexMode::Full)
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
            Some((last_sha, _)) if is_ancestor(root, &last_sha, &head_sha) => {
                IndexMode::Incremental
            }
            _ => IndexMode::Full,
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

    db.clear_git_data()?;

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

    for (path, stats) in &files {
        db.upsert_git_file(
            path,
            stats.churn_score,
            stats.fix_count,
            stats.total_commits,
            stats.last_commit_at,
        )?;
    }

    let mut co_change_kept = 0usize;
    for ((a, b), stats) in &pairs {
        if stats.count >= MIN_CO_CHANGE_COUNT {
            db.upsert_git_co_change(a, b, stats.weight, stats.count, stats.last_observed_at)?;
            co_change_kept += 1;
        }
    }

    db.set_git_index_state(head_sha)?;

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

    // Age existing rows forward by the time elapsed since last index. Exponential decay
    // composes multiplicatively: e^(-(now - t)/τ) = e^(-(last - t)/τ) * e^(-(now-last)/τ),
    // so a single global scale is mathematically exact.
    let delta_seconds = (now - last_indexed_at).max(0) as f64;
    if delta_seconds > 0.0 {
        let factor = (-delta_seconds / (DECAY_HALFLIFE_DAYS * SECONDS_PER_DAY)).exp();
        db.scale_git_decay(factor)?;
    }

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

    for (path, stats) in &files {
        db.incr_git_file(
            path,
            stats.churn_score,
            stats.fix_count,
            stats.total_commits,
            stats.last_commit_at,
        )?;
    }

    let mut co_change_kept = 0usize;
    for ((a, b), stats) in &pairs {
        // Surface a pair if either it already exists in DB (accumulate onto it) or
        // its delta alone cleared the min-count filter. Sub-threshold pairs that
        // straddle the boundary will be caught by the next full rescan.
        let should_upsert = stats.count >= MIN_CO_CHANGE_COUNT || db.co_change_pair_exists(a, b)?;
        if should_upsert {
            db.incr_git_co_change(a, b, stats.weight, stats.count, stats.last_observed_at)?;
            co_change_kept += 1;
        }
    }

    db.set_git_index_state(head_sha)?;

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

    // Drop test-like files from co-change pair generation: they pair with
    // everything they cover and would dominate coupling rankings. They stay in
    // `git_files` so the test-discovery tools can find them.
    let pairable: Vec<&&FileChange> = kept_changes
        .iter()
        .filter(|c| !test_like_set.is_match(&c.path))
        .collect();

    if pairable.len() <= MAX_FILES_PER_COMMIT_FOR_COCHANGE {
        for i in 0..pairable.len() {
            for j in (i + 1)..pairable.len() {
                let a = &pairable[i].path;
                let b = &pairable[j].path;
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

fn is_ancestor(root: &Path, old: &str, new: &str) -> bool {
    // Also confirms old still exists in the repo. If it doesn't resolve, git returns
    // nonzero, and we treat that as "not an ancestor" and fall back to full.
    Command::new("git")
        .args(["merge-base", "--is-ancestor", old, new])
        .current_dir(root)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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

    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("commit\t") {
            if let Some(prev) = current.take() {
                commits.push(prev);
            }
            let mut parts = rest.splitn(3, '\t');
            let _sha = parts.next().unwrap_or("");
            let ts: i64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
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
        // Rename detection in numstat looks like `path/{old => new}/file`. Normalize
        // to the destination by stripping `{old => ` and `}`.
        let normalized = normalize_rename_path(path);
        let added: u32 = added_s.parse().unwrap_or(0);
        let deleted: u32 = deleted_s.parse().unwrap_or(0);
        commit.changes.push(FileChange {
            path: normalized,
            added,
            deleted,
        });
    }
    if let Some(prev) = current.take() {
        commits.push(prev);
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

/// Top-N files that historically co-change with `file_path`. Returns the OTHER file in
/// each pair, weight-sorted descending. Backed by the `git_co_changes` table populated
/// by `git_history_index`.
pub fn find_coupling(db: &Database, file_path: &str, limit: usize) -> Result<Vec<CoChangeEntry>> {
    let rows = db.co_changes_for(file_path, limit)?;
    Ok(rows
        .into_iter()
        .map(|r| CoChangeEntry {
            file: r.file,
            weight: r.weight,
            count: r.count,
            last_observed_at: r.last_observed_at,
        })
        .collect())
}

/// Risk score for a single file. Composes:
/// - churn percentile (0..1) — weight 0.35
/// - fix ratio (fix_count / total_commits, capped at 1.0) — weight 0.20
/// - dependent file pressure (capped via 20 dependents) — weight 0.15
/// - coupled file pressure (capped via 10 coupled) — weight 0.15
/// - test gap (no test among coupled or as adjacent file) — weight 0.15
///
/// Output includes the decomposition so the agent can quote specific signals
/// in PR descriptions or risk callouts. Empty git history → score=0 with a note.
pub fn assess_risk(db: &Database, file_path: &str) -> Result<RiskAssessment> {
    let git = db.git_file(file_path)?;
    let churn_score = git.as_ref().map(|g| g.churn_score).unwrap_or(0.0);
    let total_commits = git.as_ref().map(|g| g.total_commits).unwrap_or(0);
    let fix_count = git.as_ref().map(|g| g.fix_count).unwrap_or(0);
    let churn_percentile = db.churn_percentile(file_path)?;
    let fix_ratio = if total_commits > 0 {
        (fix_count as f64 / total_commits as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };

    let coupled = db.co_changes_for(file_path, 10)?;
    let coupled_files = coupled.len() as u32;
    let top_coupled: Vec<CoChangeEntry> = coupled
        .into_iter()
        .map(|r| CoChangeEntry {
            file: r.file,
            weight: r.weight,
            count: r.count,
            last_observed_at: r.last_observed_at,
        })
        .collect();

    // Reverse-dependency pressure via existing impact analysis (depth=2).
    let dependent_files = impact_analysis(
        db,
        &ImpactRequest {
            target: ImpactTarget::File {
                path: file_path.to_string(),
            },
            depth: 2,
            source_only: true,
        },
    )
    .map(|v| v.len() as u32)
    .unwrap_or(0);

    // Test gap: do any coupled files look like tests, or does a sibling file matching
    // common test conventions exist in the index?
    let has_coupled_test = top_coupled
        .iter()
        .any(|e| matches!(FileCategory::classify(&e.file), FileCategory::Test));
    let has_sibling_test = test_sibling_exists(db, file_path).unwrap_or(false);
    let test_gap = !has_coupled_test && !has_sibling_test;

    let dep_pressure = (dependent_files as f64 / 20.0).min(1.0);
    let coup_pressure = (coupled_files as f64 / 10.0).min(1.0);
    let test_gap_term = if test_gap { 1.0 } else { 0.0 };

    let score = 0.35 * churn_percentile
        + 0.20 * fix_ratio
        + 0.15 * dep_pressure
        + 0.15 * coup_pressure
        + 0.15 * test_gap_term;

    let mut notes = Vec::new();
    if git.is_none() {
        notes.push(
            "no git history for this file (file too new, or `codesage git-index` hasn't been run)"
                .to_string(),
        );
    }
    if churn_percentile >= 0.75 {
        notes.push(format!(
            "hotspot: churn percentile {:.0}%",
            churn_percentile * 100.0
        ));
    }
    if fix_ratio >= 0.4 && total_commits >= 5 {
        notes.push(format!(
            "fix-heavy: {fix_count}/{total_commits} commits ({:.0}%) tagged as fixes",
            fix_ratio * 100.0
        ));
    }
    if dependent_files >= 10 {
        notes.push(format!(
            "wide blast radius: {dependent_files} files depend on this (depth-2)"
        ));
    }
    if coupled_files >= 5 {
        notes.push(format!(
            "high coupling: {coupled_files} files historically change with this"
        ));
    }
    if test_gap {
        notes.push("test gap: no obvious test file (sibling or co-changer)".to_string());
    }

    Ok(RiskAssessment {
        file: file_path.to_string(),
        score,
        churn_score,
        churn_percentile,
        fix_ratio,
        total_commits,
        fix_count,
        dependent_files,
        coupled_files,
        test_gap,
        top_coupled,
        notes,
    })
}

/// All sibling test files for `file_path` that exist in the index, by language
/// convention. Used by `recommend_tests` and (via .is_empty()) by `assess_risk`.
fn test_sibling_paths(db: &Database, file_path: &str) -> Result<Vec<String>> {
    let stem = file_path
        .rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.'))
        .map(|(s, _)| s.to_string())
        .unwrap_or_default();
    if stem.is_empty() {
        return Ok(Vec::new());
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
        if db.git_file(&normalized)?.is_some() {
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
        for path in db.git_files_with_prefix(&tests_prefix)? {
            if path.ends_with(".rs") && !path.contains("/fixtures/") && !found.contains(&path) {
                found.push(path);
            }
        }
    }
    // Workspace-root case: src/foo.rs paired with tests/*.rs at the same level.
    if file_path.ends_with(".rs") && file_path.starts_with("src/") {
        for path in db.git_files_with_prefix("tests/")? {
            if path.ends_with(".rs") && !path.contains("/fixtures/") && !found.contains(&path) {
                found.push(path);
            }
        }
    }

    Ok(found)
}

/// Heuristic: do any indexed files look like tests for `file_path`?
fn test_sibling_exists(db: &Database, file_path: &str) -> Result<bool> {
    Ok(!test_sibling_paths(db, file_path)?.is_empty())
}

/// Aggregate `assess_risk` across the file list of a patch. Lets an agent
/// ask one question instead of N round-trips. Output exposes both the
/// per-file decomposition and patch-level rollups (max/mean, files in each
/// risk category, paste-ready summary notes).
pub fn assess_risk_diff(db: &Database, file_paths: &[String]) -> Result<RiskDiffAssessment> {
    if file_paths.is_empty() {
        return Ok(RiskDiffAssessment::default());
    }

    let files: Vec<RiskAssessment> = file_paths
        .iter()
        .map(|p| assess_risk(db, p))
        .collect::<Result<Vec<_>>>()?;

    let max_score = files.iter().map(|f| f.score).fold(0.0_f64, f64::max);
    let mean_score = files.iter().map(|f| f.score).sum::<f64>() / files.len() as f64;
    let max_risk_file = files
        .iter()
        .max_by(|a, b| {
            a.score
                .partial_cmp(&b.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|f| f.file.clone());

    let test_gap_files: Vec<String> = files
        .iter()
        .filter(|f| f.test_gap)
        .map(|f| f.file.clone())
        .collect();

    let wide_blast_files: Vec<String> = files
        .iter()
        .filter(|f| f.dependent_files >= 10)
        .map(|f| f.file.clone())
        .collect();

    let fix_heavy_files: Vec<String> = files
        .iter()
        .filter(|f| f.fix_ratio >= 0.4 && f.total_commits >= 5)
        .map(|f| f.file.clone())
        .collect();

    let hotspot_files: Vec<String> = files
        .iter()
        .filter(|f| f.churn_percentile >= 0.75)
        .map(|f| f.file.clone())
        .collect();

    let mut summary_notes = Vec::new();
    if !hotspot_files.is_empty() {
        summary_notes.push(format!(
            "patch touches {} hotspot file(s)",
            hotspot_files.len()
        ));
    }
    if !fix_heavy_files.is_empty() {
        summary_notes.push(format!(
            "{} file(s) historically fix-heavy",
            fix_heavy_files.len()
        ));
    }
    if !test_gap_files.is_empty() {
        summary_notes.push(format!(
            "{} file(s) lack test coverage (no sibling test, no test in co-change history)",
            test_gap_files.len()
        ));
    }
    if !wide_blast_files.is_empty() {
        summary_notes.push(format!(
            "{} file(s) have wide blast radius (>=10 dependents)",
            wide_blast_files.len()
        ));
    }
    if max_score >= 0.6 {
        summary_notes.push(format!(
            "max risk score {max_score:.2}; consider smaller patch and broader test sweep"
        ));
    }

    Ok(RiskDiffAssessment {
        files,
        max_score,
        mean_score,
        max_risk_file,
        test_gap_files,
        wide_blast_files,
        fix_heavy_files,
        hotspot_files,
        summary_notes,
    })
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

    for path in file_paths {
        for sibling in test_sibling_paths(db, path)? {
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
    if primary_sorted.is_empty() && coupled.is_empty() {
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
    }

    Ok(TestRecommendations {
        primary: primary_sorted,
        coupled,
        notes,
    })
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

fn default_excludes() -> Vec<String> {
    Vec::new()
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
}
