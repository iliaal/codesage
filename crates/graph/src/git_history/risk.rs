//! Query-side risk + coupling over the `git_files` / `git_co_changes` tables.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result};
use codesage_protocol::{
    ClusteredDirectory, CoChangeEntry, CouplingReport, CycleEntry, FileCategory, ImpactRequest,
    ImpactTarget, RiskAssessment, RiskBatchAssessment, RiskDiffAssessment, TopSymbol,
};
use codesage_storage::Database;
use codesage_storage::db::CoChangeRow;

use super::tests_rec::test_sibling_exists;
use crate::impact::impact_analysis;

/// Map a storage `CoChangeRow` into the protocol `CoChangeEntry`. Shared by
/// `find_coupling` and `assess_risk`, which read the same co-change rows.
fn to_co_change_entry(r: CoChangeRow) -> CoChangeEntry {
    CoChangeEntry {
        file: r.file,
        weight: r.weight,
        count: r.count,
        last_observed_at: r.last_observed_at,
    }
}

/// Top-N files that historically co-change with `file_path`, wrapped in a
/// report that explains empty results. See [`CouplingReport`] for the
/// disambiguation an agent needs: was the file never indexed, does it have
/// history but no pair above the co-change threshold, or was the path wrong.
///
/// Schema change from the pre-0.4.1 `Vec<CoChangeEntry>` return type: callers
/// that read the MCP `find_coupling` response should now index into
/// `result.coupled` instead of treating the result as a bare array.
pub fn find_coupling(db: &Database, file_path: &str, limit: usize) -> Result<CouplingReport> {
    let rows = db.co_changes_for(file_path, limit)?;
    let coupled: Vec<CoChangeEntry> = rows.into_iter().map(to_co_change_entry).collect();

    let git = db.git_file(file_path)?;
    let file_indexed = git.is_some();
    let file_commits = git.as_ref().map(|g| g.total_commits).unwrap_or(0);

    // Note is generated only when `coupled` is empty. Distinguishes the three
    // dominant causes so an agent can decide whether to retry, try a
    // different tool, or warn the user that the index needs a refresh.
    let note = if !coupled.is_empty() {
        None
    } else if !file_indexed {
        Some(
            "file has no git history (not tracked by git, no commits yet, or path shape \
             does not match the index — verify with `codesage status` or \
             `codesage git-index --full`)"
                .to_string(),
        )
    } else if file_commits < 3 {
        Some(format!(
            "file has only {file_commits} tracked commit(s); co-change pairs need a \
             count of 3+ to be recorded (see `codesage git-index --full` to rebaseline)"
        ))
    } else {
        Some(format!(
            "file has {file_commits} commits but no co-change pair crosses the min-count \
             threshold of 3; this file typically changes in isolation"
        ))
    };

    Ok(CouplingReport {
        found: file_indexed,
        coupled,
        file_indexed,
        file_commits,
        note,
    })
}

/// Risk score for a single file. Composes:
/// - churn percentile (0..1) — weight 0.32
/// - fix ratio (fix_count / total_commits, capped at 1.0) — weight 0.18
/// - dependent file pressure (capped via 20 dependents) — weight 0.09
/// - coupled file pressure (capped via 10 coupled) — weight 0.09
/// - test gap (no test among coupled or as adjacent file) — weight 0.13
/// - cycle membership ((cycle_size - 1) / 4, capped at size 5) — weight 0.09
/// - trust boundary count (capped at 5 distinct boundaries) — weight 0.10
///
/// Output includes the decomposition so the agent can quote specific signals
/// in PR descriptions or risk callouts. Empty git history → score=0 with a note.
///
/// The seven weights sum to 1.0 so the maximum score is bounded; relative
/// shape is preserved when tuning so the structural signals (churn, fix
/// ratio) keep dominating over the security-shaped trust-boundary term.
pub fn assess_risk(db: &Database, file_path: &str) -> Result<RiskAssessment> {
    assess_risk_with_context(db, file_path, None, None)
}

fn assess_risk_with_context(
    db: &Database,
    file_path: &str,
    precomputed_cycles: Option<&[CycleEntry]>,
    precomputed_percentiles: Option<&HashMap<String, f64>>,
) -> Result<RiskAssessment> {
    let git = db.git_file(file_path)?;
    let churn_score = git.as_ref().map(|g| g.churn_score).unwrap_or(0.0);
    let total_commits = git.as_ref().map(|g| g.total_commits).unwrap_or(0);
    let fix_count = git.as_ref().map(|g| g.fix_count).unwrap_or(0);
    // A missing map entry means the file has no git_files row, which the
    // per-file query also scores as 0.0.
    let churn_percentile = match precomputed_percentiles {
        Some(map) => map.get(file_path).copied().unwrap_or(0.0),
        None => db.churn_percentile(file_path)?,
    };
    let fix_ratio = if total_commits > 0 {
        (fix_count as f64 / total_commits as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };

    let coupled = db.co_changes_for(file_path, 10)?;
    let coupled_files = coupled.len() as u32;
    let top_coupled: Vec<CoChangeEntry> = coupled.into_iter().map(to_co_change_entry).collect();

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
    .with_context(|| format!("computing dependent_files for risk({file_path})"))?
    .len() as u32;

    // Test gap: do any coupled files look like tests, or does a sibling file matching
    // common test conventions exist in the index?
    let has_coupled_test = top_coupled
        .iter()
        .any(|e| matches!(FileCategory::classify(&e.file), FileCategory::Test));
    let has_sibling_test = test_sibling_exists(db, file_path)
        .with_context(|| format!("checking sibling test for risk({file_path})"))?;
    let test_gap = !has_coupled_test && !has_sibling_test;

    let dep_pressure = (dependent_files as f64 / 20.0).min(1.0);
    let coup_pressure = (coupled_files as f64 / 10.0).min(1.0);
    let test_gap_term = if test_gap { 1.0 } else { 0.0 };

    // Cycle membership: best-effort. If SCC computation fails (DB-level
    // edge enumeration error), we log and continue with no cycle data
    // rather than failing the whole risk call. The structural sensor is
    // additive to the existing git/coupling signals, not load-bearing.
    let (in_cycle, cycle_size, cycle_files) = if let Some(cycles) = precomputed_cycles {
        cycle_membership(cycles, file_path)
    } else {
        match find_cycles_touching(db, &[file_path.to_string()]) {
            Ok(cycles) => cycle_membership(&cycles, file_path),
            Err(e) => {
                tracing::warn!(error = %e, file = %file_path, "cycle detection failed; omitting cycle signal from risk score");
                (false, 0, Vec::new())
            }
        }
    };
    // (cycle_size - 1) / 4 clamped: 2-file cycle → 0.25, 5+ → 1.0.
    let cycle_term = if in_cycle {
        (cycle_size.saturating_sub(1) as f64 / 4.0).min(1.0)
    } else {
        0.0
    };

    // Trust boundaries are a security-shaped signal: a file that talks to the
    // network AND reads secrets AND exec()s subprocesses is meaningfully more
    // risky than one that does none of those, even if its churn and tests are
    // identical. We cap at 5 distinct boundaries so a few extreme files
    // (legitimately broad infra glue) don't get pinned at the top of the
    // ranking solely on this term.
    let trust_boundaries = db
        .trust_boundaries_for_file_path(file_path)
        .with_context(|| format!("loading trust boundaries for risk({file_path})"))?;
    let trust_boundary_term = (trust_boundaries.len() as f64 / 5.0).min(1.0);

    let score = 0.32 * churn_percentile
        + 0.18 * fix_ratio
        + 0.09 * dep_pressure
        + 0.09 * coup_pressure
        + 0.13 * test_gap_term
        + 0.09 * cycle_term
        + 0.10 * trust_boundary_term;

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
    if in_cycle {
        // Sample up to 5 other members for the rationale line so the note
        // stays short on big cycles; the full list is in `cycle_files`.
        const NOTE_SAMPLE: usize = 5;
        let sample: Vec<&str> = cycle_files
            .iter()
            .take(NOTE_SAMPLE)
            .map(|s| s.as_str())
            .collect();
        let extra = cycle_files.len().saturating_sub(NOTE_SAMPLE);
        let suffix = if extra > 0 {
            format!(" (+{extra} more)")
        } else {
            String::new()
        };
        notes.push(format!(
            "in import cycle of {} files: {}{suffix}",
            cycle_size,
            sample.join(", ")
        ));

        // Cycle-breaking guidance, shaped by the cycle's structure. A single
        // edge removal only meaningfully breaks an SCC close to a simple ring
        // (each file imported by ~one other cycle member, edges ≈ nodes); there
        // the lowest-co-change edge is the most arbitrary coupling and the
        // safest dependency to invert. When the SCC has a hub many cycle files
        // import, or far more edges than a ring, one cut won't untangle it — so
        // surface the most-depended-on files (the decoupling targets) instead.
        // `cycle_files` excludes the current file, so add it back for the SCC.
        let mut scc: Vec<&str> = cycle_files.iter().map(String::as_str).collect();
        scc.push(file_path);
        match db.import_edges_within(&scc) {
            Ok(edges) if !edges.is_empty() => {
                let mut in_degree: std::collections::HashMap<&str, u32> =
                    std::collections::HashMap::new();
                for (_from, to) in &edges {
                    *in_degree.entry(to.as_str()).or_insert(0) += 1;
                }
                let max_in_degree = in_degree.values().copied().max().unwrap_or(0);
                let ring_like = max_in_degree <= 2 && edges.len() <= scc.len() + scc.len() / 2;
                if ring_like {
                    let weakest = edges
                        .iter()
                        .map(|(from, to)| (db.co_change_weight(from, to).unwrap_or(0.0), from, to))
                        .min_by(|(wa, fa, ta), (wb, fb, tb)| {
                            wa.partial_cmp(wb)
                                .unwrap_or(std::cmp::Ordering::Equal)
                                .then_with(|| fa.cmp(fb))
                                .then_with(|| ta.cmp(tb))
                        });
                    if let Some((weight, from, to)) = weakest {
                        if weight > 0.0 {
                            notes.push(format!(
                                "candidate break point: {from} → {to} (lowest co-change weight {weight:.2} among cycle edges)"
                            ));
                        } else {
                            notes.push(format!(
                                "candidate break point: {from} → {to} (these cycle files do not co-change in git history)"
                            ));
                        }
                    }
                } else {
                    let mut ranked: Vec<(&str, u32)> = in_degree.into_iter().collect();
                    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
                    let hubs: Vec<&str> = ranked.iter().take(3).map(|(f, _)| *f).collect();
                    notes.push(format!(
                        "cycle is hub-dominated (not a simple ring); cutting one edge won't break it — most-depended-on within the cycle (decoupling targets): {}",
                        hubs.join(", ")
                    ));
                }
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, file = %file_path, "cycle-break guidance failed; omitting from risk notes");
            }
        }
    }
    if trust_boundaries.len() >= 3 {
        let names: Vec<&str> = trust_boundaries.iter().map(|b| b.as_str()).collect();
        notes.push(format!(
            "crosses {} trust boundaries ({}) — security review recommended",
            trust_boundaries.len(),
            names.join(", ")
        ));
    }

    let top_symbols = match compute_top_symbols(db, file_path, in_cycle, cycle_size) {
        Ok(v) => v,
        Err(e) => {
            // Failing to fetch symbols shouldn't fail the whole risk call; the
            // top-symbol breakdown is additive context, not load-bearing.
            tracing::warn!(error = %e, file = %file_path, "top-symbols computation failed; omitting from risk");
            Vec::new()
        }
    };

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
        in_cycle,
        cycle_size,
        cycle_files,
        top_coupled,
        trust_boundaries,
        notes,
        top_symbols,
    })
}

fn cycle_membership(cycles: &[CycleEntry], file_path: &str) -> (bool, u32, Vec<String>) {
    cycles
        .iter()
        .find(|c| c.members.iter().any(|m| m == file_path))
        .map(|c| {
            let others: Vec<String> = c
                .members
                .iter()
                .filter(|m| m.as_str() != file_path)
                .cloned()
                .collect();
            (true, c.size, others)
        })
        .unwrap_or((false, 0, Vec::new()))
}

/// Maximum symbols returned per file. Burn-budget protection: a file with 200
/// methods still returns 5 entries — agents asking "what drives this file's
/// score" don't need the long tail.
const TOP_SYMBOLS_CAP: usize = 5;

/// Rank symbols inside `file_path` by the heuristic
/// `ln(1 + line_count) + ref_count + (in_cycle ? 1.0 : 0.0)` and return the
/// top [`TOP_SYMBOLS_CAP`] with a one-line `why`. Cycle membership is a
/// file-level signal: every symbol in a file participating in an import cycle
/// gets the same +1.0 bump, which is the intended behaviour — the cycle term
/// promotes hot files into the top-symbols breakdown without distorting the
/// intra-file ordering.
///
/// Empty when the file has no indexed symbols. Not an error.
fn compute_top_symbols(
    db: &codesage_storage::Database,
    file_path: &str,
    in_cycle: bool,
    cycle_size: u32,
) -> Result<Vec<TopSymbol>> {
    let symbols = db
        .symbols_for_file(file_path)
        .with_context(|| format!("loading symbols for top-symbols breakdown of {file_path}"))?;
    if symbols.is_empty() {
        return Ok(Vec::new());
    }

    // One batched ref-count query for every symbol in the file. Refs match by
    // short name (and tail-name fallback for qualified callsites) — same shape
    // as `find_references`.
    let names: Vec<String> = symbols.iter().map(|s| s.name.clone()).collect();
    let counts = db
        .reference_counts_for_names(&names)
        .with_context(|| format!("counting refs for top-symbols breakdown of {file_path}"))?;

    let cycle_bonus = if in_cycle { 1.0_f64 } else { 0.0 };

    let mut scored: Vec<(f64, &codesage_protocol::Symbol, u32)> = symbols
        .iter()
        .map(|s| {
            let line_count = s.line_end.saturating_sub(s.line_start).saturating_add(1);
            let ref_count = counts.get(&s.name).copied().unwrap_or(0);
            let score = (1.0 + line_count as f64).ln() + ref_count as f64 + cycle_bonus;
            (score, s, ref_count)
        })
        .collect();

    // Descending by score. Stable sort keeps insertion order (=source order)
    // as a deterministic tiebreaker so two equal-scored symbols always come
    // out in the same order.
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.line_start.cmp(&b.1.line_start))
    });

    Ok(scored
        .into_iter()
        .take(TOP_SYMBOLS_CAP)
        .map(|(_, sym, ref_count)| {
            let line_count = sym
                .line_end
                .saturating_sub(sym.line_start)
                .saturating_add(1);
            let cycle_clause = if in_cycle {
                format!(", in {cycle_size}-file cycle")
            } else {
                String::new()
            };
            let why = format!("hot: {line_count} lines, {ref_count} refs{cycle_clause}");
            TopSymbol {
                name: sym.name.clone(),
                line: sym.line_start,
                kind: sym.kind.as_str().to_string(),
                why,
            }
        })
        .collect())
}

/// Aggregate `assess_risk` across the file list of a patch. Lets an agent
/// ask one question instead of N round-trips. Output exposes both the
/// per-file decomposition and patch-level rollups (max/mean, files in each
/// risk category, paste-ready summary notes).
pub fn assess_risk_diff(db: &Database, file_paths: &[String]) -> Result<RiskDiffAssessment> {
    if file_paths.is_empty() {
        return Ok(RiskDiffAssessment::default());
    }

    // Cycles are graph-wide SCCs; compute once for the patch, then reuse the
    // result for per-file scores and the patch-level cycle list.
    let cycles_touching_patch = match find_cycles_touching(db, file_paths) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "cycle detection failed; omitting cycles_touching_patch");
            Vec::new()
        }
    };

    // Churn percentiles rank each file against the whole git_files table;
    // one window-function query replaces a full-table aggregate per file.
    let percentiles = db
        .churn_percentiles()
        .context("bulk churn percentiles for risk diff")?;

    let files: Vec<RiskAssessment> = file_paths
        .iter()
        .map(|p| assess_risk_with_context(db, p, Some(&cycles_touching_patch), Some(&percentiles)))
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
    // A hotspot+fix-heavy+test-gap file with no trust boundaries hits the
    // weight ceiling at ~0.54, so 0.50 catches "worth flagging" without
    // firing on every mid-tier file.
    if max_score >= 0.50 {
        summary_notes.push(format!(
            "max risk score {max_score:.2}; consider smaller patch and broader test sweep"
        ));
    }

    if !cycles_touching_patch.is_empty() {
        let biggest = cycles_touching_patch
            .iter()
            .map(|c| c.size)
            .max()
            .unwrap_or(0);
        summary_notes.push(format!(
            "{} import cycle(s) involve patch files (largest: {} files)",
            cycles_touching_patch.len(),
            biggest
        ));
    }

    let (mut files, clustered_directories) = cluster_by_directory(files, DIR_CLUSTER_THRESHOLD);

    // Alias categorical notes that repeat across files into short codes.
    // The clustered files have already been demoted to `omitted_files` (no
    // notes there), so we only alias the kept `files[]` entries plus the
    // detail kept inside each cluster's `top_files`.
    let mut all_for_alias: Vec<&mut RiskAssessment> = files.iter_mut().collect();
    let mut clustered_directories = clustered_directories;
    for cd in clustered_directories.iter_mut() {
        for f in cd.top_files.iter_mut() {
            all_for_alias.push(f);
        }
    }
    let legend = alias_categorical_notes_in_place(&mut all_for_alias);

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
        clustered_directories,
        cycles_touching_patch,
        legend,
    })
}

/// `assess_risk` over a list of files, returning per-file decomposition
/// without patch-level aggregation. See [`RiskBatchAssessment`] for the
/// design distinction vs [`assess_risk_diff`].
///
/// Retrospective session analysis (recommendations doc §1.7, 30-day
/// window) found 230 individual `assess_risk` MCP calls vs 13
/// `assess_risk_diff` — the agent's dominant pattern is per-file scoring,
/// not patch aggregation. This batch variant cuts the per-call MCP
/// protocol overhead for that pattern: one round-trip for N files.
pub fn assess_risk_batch(db: &Database, file_paths: &[String]) -> Result<RiskBatchAssessment> {
    if file_paths.is_empty() {
        return Ok(RiskBatchAssessment::default());
    }
    let cycles = match find_cycles_touching(db, file_paths) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "cycle detection failed; omitting batch cycle signal");
            Vec::new()
        }
    };
    let percentiles = db
        .churn_percentiles()
        .context("bulk churn percentiles for risk batch")?;
    let mut files: Vec<RiskAssessment> = file_paths
        .iter()
        .map(|p| assess_risk_with_context(db, p, Some(&cycles), Some(&percentiles)))
        .collect::<Result<Vec<_>>>()?;
    let mut refs: Vec<&mut RiskAssessment> = files.iter_mut().collect();
    let legend = alias_categorical_notes_in_place(&mut refs);
    Ok(RiskBatchAssessment { files, legend })
}

/// Categorical notes eligible for aliasing into a top-level `_legend`.
/// Templated notes (those with formatted percentages, counts, or file
/// lists) are not eligible because they collide across files. Order
/// here is also the deterministic short-code order: the first eligible
/// match gets `T`, next gets `NG`, etc., so output is stable.
const ALIASABLE_NOTES: &[(&str, &str)] = &[
    (
        "T",
        "test gap: no obvious test file (sibling or co-changer)",
    ),
    (
        "NG",
        "no git history for this file (file too new, or `codesage git-index` hasn't been run)",
    ),
];

/// In-place alias of categorical notes that appear in ≥3 files of the
/// input, returning the resulting short-code → full-string legend.
///
/// Threshold reasoning: the `_legend` entry itself costs ~75-95 bytes;
/// each replaced note saves ~50-90 bytes minus the 3-byte code. Net
/// savings turn positive at 3 occurrences for the longer note, 2 for
/// the shorter. Picking 3 as the floor for both keeps the rule simple
/// and ensures the worst case is still net-positive.
fn alias_categorical_notes_in_place(files: &mut [&mut RiskAssessment]) -> BTreeMap<String, String> {
    let mut legend = BTreeMap::new();
    if files.len() < 3 {
        return legend;
    }
    for (code, full) in ALIASABLE_NOTES {
        let count = files
            .iter()
            .filter(|f| f.notes.iter().any(|n| n == full))
            .count();
        if count < 3 {
            continue;
        }
        legend.insert((*code).to_string(), (*full).to_string());
        for f in files.iter_mut() {
            for note in f.notes.iter_mut() {
                if note == full {
                    *note = (*code).to_string();
                }
            }
        }
    }
    legend
}

/// Find strongly-connected components in the file-level import graph
/// that contain at least one file from `patch_files`. Returns `CycleEntry`
/// rows sorted by (descending size, then alphabetical) for stable output.
///
/// See [`CycleEntry`] docs for the "cycles the patch touches" vs
/// "cycles the patch introduces" distinction. We do not have a
/// pre-patch index to diff against, so this returns both.
fn find_cycles_touching(db: &Database, patch_files: &[String]) -> Result<Vec<CycleEntry>> {
    use std::collections::HashSet;

    let edges = db
        .enumerate_file_import_edges()
        .with_context(|| "enumerate_file_import_edges")?;
    if edges.is_empty() {
        return Ok(Vec::new());
    }
    let patch: HashSet<&str> = patch_files.iter().map(|s| s.as_str()).collect();
    let components = crate::scc::tarjan_scc(&edges);
    let mut out: Vec<CycleEntry> = Vec::new();
    for component in components {
        // Trivial SCCs (single-node, no self-edge) aren't cycles.
        if component.len() < 2 {
            continue;
        }
        if !component.iter().any(|f| patch.contains(f.as_str())) {
            continue;
        }
        let max_churn_file = pick_max_churn(db, &component)?;
        let mut members = component;
        members.sort();
        let size = members.len() as u32;
        out.push(CycleEntry {
            members,
            size,
            max_churn_file,
        });
    }
    // Largest cycles first — most useful for an agent scanning the output.
    out.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.members.cmp(&b.members)));
    Ok(out)
}

/// Return the member with the highest `churn_score` in `git_files`, or
/// `None` if none of the members have a git history row. Best-effort
/// heuristic: the most-modified member tends to be the most-crosscut
/// and is usually the right refactor site to break the cycle.
fn pick_max_churn(db: &Database, members: &[String]) -> Result<Option<String>> {
    let mut best: Option<(f64, String)> = None;
    for m in members {
        if let Some(row) = db.git_file(m)? {
            match &best {
                Some((score, _)) if row.churn_score <= *score => {}
                _ => best = Some((row.churn_score, m.clone())),
            }
        }
    }
    Ok(best.map(|(_, f)| f))
}

/// When a patch touches at least this many files in a single directory, the
/// per-file detail for that directory is condensed into a `ClusteredDirectory`
/// entry. Measured on real 30-day session logs: `assess_risk_diff` responses
/// at p95 were 24 KB and saved ~13% with this rule; smaller patches are
/// untouched so agent prompts built against the flat shape keep working.
const DIR_CLUSTER_THRESHOLD: usize = 5;

/// Group `files` by their parent directory. Any directory with
/// `>= threshold` entries is collapsed to a `ClusteredDirectory` whose
/// `top_files` keep full detail for the three highest-scoring files and
/// whose `omitted_files` lists the rest by name. Directories below the
/// threshold are returned unchanged in the first tuple element.
fn cluster_by_directory(
    files: Vec<RiskAssessment>,
    threshold: usize,
) -> (Vec<RiskAssessment>, Vec<ClusteredDirectory>) {
    use std::collections::BTreeMap;

    // Bucket by parent directory, preserving insertion order inside each bucket.
    let mut buckets: BTreeMap<String, Vec<RiskAssessment>> = BTreeMap::new();
    for f in files {
        let dir = std::path::Path::new(&f.file)
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or("")
            .to_string();
        buckets.entry(dir).or_default().push(f);
    }

    let mut kept: Vec<RiskAssessment> = Vec::new();
    let mut clusters: Vec<ClusteredDirectory> = Vec::new();
    for (dir, mut items) in buckets {
        if items.len() < threshold {
            kept.extend(items);
            continue;
        }
        // Sort by risk score descending so top-3 are the highest.
        items.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let count = items.len() as u32;
        let top_files: Vec<RiskAssessment> = items.iter().take(3).cloned().collect();
        let omitted_files: Vec<String> = items.iter().skip(3).map(|f| f.file.clone()).collect();
        clusters.push(ClusteredDirectory {
            directory: dir,
            count,
            top_files,
            omitted_files,
        });
    }
    (kept, clusters)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Small indexed project so impact_analysis has a graph to walk; mirrors
    /// the setup in `tests/risk_test.rs`.
    fn setup_project() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("Repository.php"),
            b"<?php\nnamespace App;\nclass Repository {\n  public function find($id) { return null; }\n}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("Controller.php"),
            b"<?php\nnamespace App;\nuse App\\Repository;\nclass Controller {\n  public function show(Repository $r, $id) { return $r->find($id); }\n}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("Service.php"),
            b"<?php\nnamespace App;\nuse App\\Repository;\nclass Service {\n  public function run(Repository $r) { return $r->find(1); }\n}\n",
        )
        .unwrap();
        let db = Database::open_in_memory().unwrap();
        crate::full_index(root, &db, &[], false).unwrap();
        (dir, db)
    }

    #[test]
    fn batch_scores_bit_identical_to_single_file_path() {
        let (_dir, db) = setup_project();
        // Tied churn scores exercise the CUME_DIST tie handling end-to-end;
        // Service.php has no git row so its percentile takes the 0.0 fallback
        // on both paths.
        db.upsert_git_file("Repository.php", 10.0, 4, 8, Some(1_700_000_000))
            .unwrap();
        db.upsert_git_file("Controller.php", 10.0, 1, 8, Some(1_700_000_000))
            .unwrap();
        db.upsert_git_file("other_a.php", 10.0, 0, 5, Some(1_700_000_000))
            .unwrap();
        db.upsert_git_file("other_b.php", 0.5, 0, 2, Some(1_700_000_000))
            .unwrap();

        let files: Vec<String> = [
            "Repository.php",
            "Controller.php",
            "Service.php",
            "other_b.php",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let batch = assess_risk_batch(&db, &files).unwrap();
        assert_eq!(batch.files.len(), files.len());
        for (path, batched) in files.iter().zip(batch.files.iter()) {
            let single = assess_risk(&db, path).unwrap();
            assert_eq!(&single.file, path);
            assert_eq!(&batched.file, path);
            assert_eq!(
                single.score.to_bits(),
                batched.score.to_bits(),
                "{path}: single score {} != batch score {}",
                single.score,
                batched.score
            );
            assert_eq!(
                single.churn_percentile.to_bits(),
                batched.churn_percentile.to_bits(),
                "{path}: single percentile {} != batch percentile {}",
                single.churn_percentile,
                batched.churn_percentile
            );
            assert_eq!(single.in_cycle, batched.in_cycle, "{path}: cycle flag");
            assert_eq!(single.cycle_size, batched.cycle_size, "{path}: cycle size");
        }
    }
}
