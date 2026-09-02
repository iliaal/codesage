//! Git-history intelligence commands: `git-index`, `coupling`, `risk`,
//! `risk-batch`, `risk-diff`, `tests-for`, `rehearse`.

use std::path::Path;
use std::time::Duration;

use anyhow::{Result, bail};
use codesage_graph::{assess_risk, find_coupling};

use crate::{
    acquire_index_lock, find_project_root, get_user_exclude_patterns, load_project_config, open_db,
};

pub(crate) fn cmd_git_index(
    json: bool,
    full: bool,
    incremental: bool,
    lock_wait: Duration,
) -> Result<()> {
    let root = find_project_root()?;
    // Same lock as `codesage index`: if a structural index is in flight,
    // the git-history pass would race it and hit SQLITE_BUSY. Skipping
    // here lets the hook-driven scheduler converge on a single indexer
    // at a time without the user seeing an error. `--lock-wait` bounds
    // a polling wait first — the watcher never refreshes git history, so
    // a hook-invoked skip would leave it stale until the next commit.
    let _lock = acquire_index_lock(&root, "skipping", lock_wait)?;
    let db = open_db(&root)?;
    let config = load_project_config(&root)?;
    let excludes = get_user_exclude_patterns(&config);
    let mode = if full {
        codesage_graph::IndexMode::Full
    } else if incremental {
        codesage_graph::IndexMode::Incremental
    } else {
        codesage_graph::IndexMode::Auto
    };
    let stats = codesage_graph::git_history_index_with_options(&db, &root, &excludes, mode)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&stats)?);
    } else {
        println!(
            "Git history indexed ({mode:?}): commits_scanned={} files_tracked={} co_change_pairs={}",
            stats.commits_scanned, stats.files_tracked, stats.co_change_pairs
        );
    }
    Ok(())
}

pub(crate) fn cmd_coupling(file: &str, limit: usize, json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;
    let report = find_coupling(&db, file, limit)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if report.coupled.is_empty() {
        let hint = report
            .note
            .as_deref()
            .unwrap_or("no co-change history; run `codesage git-index` or check the path");
        println!("No co-change history for {file}: {hint}");
    } else {
        println!(
            "Files that historically change with {file} ({} commits tracked):",
            report.file_commits
        );
        for e in &report.coupled {
            println!("  {:>6.2}  {:>4}x  {}", e.weight, e.count, e.file);
        }
    }
    Ok(())
}

pub(crate) fn cmd_risk(file: &str, json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;
    let assessment = assess_risk(&db, file)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&assessment)?);
    } else {
        println!(
            "Risk: {} (score: {:.2}/1.00)",
            assessment.file, assessment.score
        );
        println!(
            "  churn={:.2} (percentile {:.0}%) | fix={}/{} ({:.0}%) | dependents={} | coupled={} | test_gap={}",
            assessment.churn_score,
            assessment.churn_percentile * 100.0,
            assessment.fix_count,
            assessment.total_commits,
            assessment.fix_ratio * 100.0,
            assessment.dependent_files,
            assessment.coupled_files,
            assessment.test_gap,
        );
        if !assessment.trust_boundaries.is_empty() {
            let names: Vec<&str> = assessment
                .trust_boundaries
                .iter()
                .map(|b| b.as_str())
                .collect();
            println!("  trust_boundaries: {}", names.join(", "));
        }
        if !assessment.notes.is_empty() {
            println!("  Notes:");
            for n in &assessment.notes {
                println!("    - {n}");
            }
        }
        if !assessment.top_coupled.is_empty() {
            println!("  Top coupled:");
            for c in assessment.top_coupled.iter().take(5) {
                println!("    {:>5.2}  {}", c.weight, c.file);
            }
        }
        if !assessment.top_symbols.is_empty() {
            println!("  Top symbols:");
            for s in &assessment.top_symbols {
                println!("    L{:<5} {} ({}) — {}", s.line, s.name, s.kind, s.why);
            }
        }
    }
    Ok(())
}

/// Resolve a file-list argument: positional args if non-empty, else newline-separated
/// from stdin. Used by `risk-diff` and `tests-for` so they compose with `git diff
/// --name-only` and similar pipelines.
fn resolve_file_list(files: Vec<String>) -> Result<Vec<String>> {
    if !files.is_empty() {
        return Ok(files);
    }
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    Ok(buf
        .lines()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect())
}

pub(crate) fn cmd_risk_diff(files: Vec<String>, json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;
    let files = resolve_file_list(files)?;
    if files.is_empty() {
        bail!("no file paths provided (pass as args or pipe via stdin)");
    }
    let assessment = codesage_graph::assess_risk_diff(&db, &files)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&assessment)?);
    } else {
        println!(
            "Patch risk: {} file(s) | max={:.2} mean={:.2}",
            assessment.files.len(),
            assessment.max_score,
            assessment.mean_score
        );
        if let Some(top) = &assessment.max_risk_file {
            println!("  highest-risk file: {top}");
        }
        for label in [
            ("hotspot", &assessment.hotspot_files),
            ("fix-heavy", &assessment.fix_heavy_files),
            ("test gap", &assessment.test_gap_files),
            ("wide blast radius", &assessment.wide_blast_files),
        ] {
            if !label.1.is_empty() {
                println!("  {} ({}):", label.0, label.1.len());
                for f in label.1 {
                    println!("    - {f}");
                }
            }
        }
        if !assessment.summary_notes.is_empty() {
            println!("  Notes:");
            for n in &assessment.summary_notes {
                println!("    - {n}");
            }
        }
    }
    Ok(())
}

pub(crate) fn cmd_risk_batch(files: Vec<String>, json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;
    let files = resolve_file_list(files)?;
    if files.is_empty() {
        bail!("no file paths provided (pass as args or pipe via stdin)");
    }
    let assessment = codesage_graph::assess_risk_batch(&db, &files)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&assessment)?);
    } else {
        println!("Per-file risk: {} file(s)", assessment.files.len());
        for f in &assessment.files {
            println!("  {:>5.2}  {}", f.score, f.file);
        }
        if !assessment.legend.is_empty() {
            println!("  Legend:");
            for (code, full) in &assessment.legend {
                println!("    {code} = {full}");
            }
        }
    }
    Ok(())
}

pub(crate) fn cmd_tests_for(files: Vec<String>, json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;
    let files = resolve_file_list(files)?;
    if files.is_empty() {
        bail!("no file paths provided (pass as args or pipe via stdin)");
    }
    let recs = codesage_graph::recommend_tests(&db, &files)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&recs)?);
    } else {
        if !recs.primary.is_empty() {
            println!("Primary tests (sibling convention):");
            for f in &recs.primary {
                println!("  {f}");
            }
        }
        if !recs.coupled.is_empty() {
            println!("Coupled tests (co-change history):");
            for c in &recs.coupled {
                println!(
                    "  {:>5.2}  {:>4}x  {}  (couples with {})",
                    c.weight, c.count, c.file, c.source
                );
            }
        }
        if recs.primary.is_empty() && recs.coupled.is_empty() {
            println!("No test files found for the given paths.");
        }
        if !recs.notes.is_empty() {
            for n in &recs.notes {
                println!("# {n}");
            }
        }
    }
    Ok(())
}

/// Resolve the patch file list for `rehearse`: explicit args, else piped stdin,
/// else the working-tree changes vs HEAD. Lets the command run both in a
/// pipeline (`git diff --name-only | codesage rehearse`) and bare in a dirty
/// working tree.
fn resolve_patch_files(root: &Path, files: Vec<String>) -> Result<Vec<String>> {
    if !files.is_empty() {
        return Ok(files);
    }
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return resolve_file_list(Vec::new());
    }
    working_tree_changes(root)
}

/// Files changed in the working tree relative to HEAD (tracked modifications,
/// staged or not). Empty on a clean tree or when git is unavailable.
fn working_tree_changes(root: &Path) -> Result<Vec<String>> {
    let out = std::process::Command::new("git")
        .args(["diff", "--name-only", "HEAD"])
        .current_dir(root)
        .output()?;
    if !out.status.success() {
        return Ok(Vec::new());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect())
}

pub(crate) fn cmd_rehearse(files: Vec<String>, json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;
    let files = resolve_patch_files(&root, files)?;
    if files.is_empty() {
        bail!(
            "no changed files (pass paths as args, pipe via stdin, or make working-tree changes)"
        );
    }
    let rehearsal = codesage_graph::build_review_rehearsal(&root, &db, &files)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&rehearsal)?);
        return Ok(());
    }
    println!(
        "Review rehearsal: {} file(s), {} objection(s)",
        rehearsal.files.len(),
        rehearsal.objections.len()
    );
    for o in &rehearsal.objections {
        println!("  [{}] {} — {}", o.severity.as_str(), o.category, o.title);
        for e in &o.evidence {
            println!("      {e}");
        }
    }
    if !rehearsal.summary_notes.is_empty() {
        println!("Summary:");
        for n in &rehearsal.summary_notes {
            println!("  - {n}");
        }
    }
    Ok(())
}
