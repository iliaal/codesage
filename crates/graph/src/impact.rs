use std::collections::{HashMap, HashSet};

use anyhow::Result;
use codesage_protocol::{
    CategoryCount, DistanceCount, FileCategory, ImpactEntry, ImpactOptions, ImpactReason,
    ImpactReport, ImpactRequest, ImpactSummary, ImpactTarget, Reference, SiblingSymbol, Symbol,
};
use codesage_storage::Database;

use crate::bundle::resolve_callee_definitions;

pub(crate) fn is_qualified_symbol_name(name: &str) -> bool {
    name.contains('\\') || name.contains('.') || name.contains("::")
}

/// Per-level frontier cap. A symbol referenced by hundreds of files explodes
/// the frontier and the per-symbol `references_for_symbol` queries at the next
/// depth, so each level is deduped and capped. When the cap fires, deeper
/// levels are incomplete — `impact_analysis_walk` reports that so consumers
/// (`assess_risk`'s blast-radius and test-reach checks) can say "lower bound"
/// instead of presenting a truncated count as the whole answer.
pub(crate) const MAX_FRONTIER: usize = 512;

pub fn impact_analysis(db: &Database, req: &ImpactRequest) -> Result<Vec<ImpactEntry>> {
    Ok(impact_analysis_walk(db, req, MAX_FRONTIER)?.0)
}

/// [`impact_analysis`] plus a `frontier_capped` flag: `true` when any level's
/// frontier was truncated at `max_frontier`, meaning entries at deeper
/// distances may be missing and every derived count is a lower bound.
/// `max_frontier` is a parameter only so tests can force the cap without
/// building a 512-symbol fixture.
pub(crate) fn impact_analysis_walk(
    db: &Database,
    req: &ImpactRequest,
    max_frontier: usize,
) -> Result<(Vec<ImpactEntry>, bool)> {
    let seed_symbols: Vec<Symbol> = match &req.target {
        ImpactTarget::Symbol { name } => {
            let syms = db.find_symbols(name, None)?;
            if !is_qualified_symbol_name(name) && syms.len() > 1 {
                // Only distinct qualified names are disambiguable. Languages
                // without namespaces (JS/TS) give every definition the bare
                // name, so a `.d.ts` declaration beside its `.js`
                // implementation used to produce "qualify with one of: Foo,
                // Foo" — an instruction no input can satisfy. When the names
                // collapse to one, seed on every definition and let the union
                // of dependents stand; over-inclusion is the safe direction
                // for an advisory what-to-review signal.
                let mut candidates: Vec<String> =
                    syms.iter().map(|s| s.qualified_name.clone()).collect();
                candidates.sort();
                candidates.dedup();
                if candidates.len() > 1 {
                    anyhow::bail!(
                        "ambiguous symbol '{name}': {} definitions — qualify with one of: {}, \
                         or target a single file instead",
                        syms.len(),
                        candidates.join(", ")
                    );
                }
            }
            syms
        }
        ImpactTarget::File { path } => db.symbols_for_file(path)?,
    };

    if seed_symbols.is_empty() {
        return Ok((Vec::new(), false));
    }

    let origin_files: HashSet<String> = match &req.target {
        ImpactTarget::File { path } => {
            let mut s = HashSet::new();
            s.insert(path.clone());
            s
        }
        ImpactTarget::Symbol { .. } => seed_symbols.iter().map(|s| s.file_path.clone()).collect(),
    };

    let mut file_reasons: HashMap<String, (u32, Vec<ImpactReason>)> = HashMap::new();
    let mut frontier: Vec<Symbol> = seed_symbols;
    let mut visited_symbols: HashSet<(String, String, u32)> = HashSet::new();
    let mut frontier_capped = false;

    for depth in 1..=req.depth as u32 {
        // First pass: collect refs, update file_reasons, record (from_file, line) pairs
        // that need caller-symbol lookups for the next frontier.
        let mut pending_callers: Vec<(String, Option<String>, u32)> = Vec::new();
        for sym in &frontier {
            if !visited_symbols.insert(symbol_identity_key(sym)) {
                continue;
            }
            let refs = references_for_symbol(db, sym)?;
            for r in refs {
                if origin_files.contains(&r.from_file) {
                    continue;
                }
                let entry = file_reasons
                    .entry(r.from_file.clone())
                    .or_insert_with(|| (depth, Vec::new()));
                if entry.0 > depth {
                    entry.0 = depth;
                }
                // Seeding on several definitions that share one qualified name
                // walks the same reference row once per definition, so the
                // identical reason arrives repeatedly. Reason count feeds the
                // ranking below, which would let a duplicate decide which files
                // survive a result limit.
                let reason = ImpactReason {
                    via_symbol: sym.name.clone(),
                    kind: r.kind,
                    line: r.line,
                };
                let already = entry.1.iter().any(|e| {
                    e.via_symbol == reason.via_symbol
                        && e.kind == reason.kind
                        && e.line == reason.line
                });
                if !already && entry.1.len() < 10 {
                    entry.1.push(reason);
                }
                if depth < req.depth as u32 {
                    pending_callers.push((r.from_file, r.from_symbol, r.line));
                }
            }
        }

        if pending_callers.is_empty() {
            break;
        }

        // Batched caller-symbol lookup: one query per distinct file, regardless of
        // how many lines in that file triggered the lookup.
        let distinct_files: Vec<String> = {
            let mut set: HashSet<String> = HashSet::new();
            pending_callers.iter().for_each(|(f, _, _)| {
                set.insert(f.clone());
            });
            set.into_iter().collect()
        };
        let syms_by_file = db.symbols_for_files(&distinct_files)?;

        let mut next_frontier: Vec<Symbol> = Vec::new();
        for (from_file, from_symbol, line) in &pending_callers {
            let Some(syms) = syms_by_file.get(from_file) else {
                continue;
            };
            // Precise path: the reference recorded its enclosing symbol, so jump
            // straight to that one symbol instead of every symbol spanning the
            // line (which conflated a method with its containing class).
            if let Some(qn) = from_symbol
                && let Some(s) = syms.iter().find(|s| &s.qualified_name == qn)
            {
                next_frontier.push(s.clone());
                continue;
            }
            // Fallback for references with no recorded enclosing symbol: the
            // innermost symbol whose range contains the line.
            let mut best: Option<&Symbol> = None;
            for s in syms {
                if s.line_start <= *line && s.line_end >= *line {
                    let span = s.line_end - s.line_start;
                    match best {
                        Some(b) if (b.line_end - b.line_start) <= span => {}
                        _ => best = Some(s),
                    }
                }
            }
            if let Some(s) = best {
                next_frontier.push(s.clone());
            }
        }

        // Bound fan-out: dedup by qualified name and cap each level so a wide
        // blast radius can't make impact analysis unbounded (see
        // [`MAX_FRONTIER`]).
        let mut seen_symbols: HashSet<(String, String, u32)> = HashSet::new();
        next_frontier.retain(|s| seen_symbols.insert(symbol_identity_key(s)));
        if next_frontier.len() > max_frontier {
            tracing::debug!(
                frontier = next_frontier.len(),
                cap = max_frontier,
                depth,
                "impact_analysis frontier capped"
            );
            next_frontier.truncate(max_frontier);
            frontier_capped = true;
        }

        if next_frontier.is_empty() {
            break;
        }
        frontier = next_frontier;
    }

    let mut entries: Vec<ImpactEntry> = file_reasons
        .into_iter()
        .map(|(path, (distance, reasons))| {
            let category = FileCategory::classify(&path);
            ImpactEntry {
                file_path: path,
                distance,
                category,
                reasons,
            }
        })
        .filter(|e| !req.source_only || e.category == FileCategory::Source)
        .collect();

    // `file_reasons` is a HashMap, so its iteration order is reseeded per map
    // instance — tied entries would otherwise land in a different order on
    // every call, and callers truncate (`ImpactOptions::limit`, the MCP budget
    // cap), so an unchanged query could return a different set of files. Ties
    // are routine here: every depth-1 file with the same reason count ties.
    entries.sort_by(|a, b| {
        a.distance
            .cmp(&b.distance)
            .then_with(|| b.reasons.len().cmp(&a.reasons.len()))
            .then_with(|| a.file_path.cmp(&b.file_path))
    });
    Ok((entries, frontier_capped))
}

/// Cap on `sibling_symbols` to keep dense files from blowing up the response.
const SIBLING_SYMBOL_CAP: usize = 60;

/// `impact_analysis` plus the adaptive extras requested via [`ImpactOptions`]:
/// forward dependencies, same-file sibling symbols, a result `limit`, and a
/// `summary_only` rollup. With all options default, the `results` field equals
/// the classic `impact_analysis` output.
pub fn impact_analysis_report(
    db: &Database,
    req: &ImpactRequest,
    opts: &ImpactOptions,
) -> Result<ImpactReport> {
    let mut entries = impact_analysis(db, req)?;

    // Summary reflects the full result set, before any limit truncation.
    let summary = if opts.summary_only {
        Some(build_impact_summary(&entries))
    } else {
        None
    };

    let mut truncated = false;
    if let Some(limit) = opts.limit
        && entries.len() > limit
    {
        entries.truncate(limit);
        truncated = true;
    }

    // Collapse each file's reason list to a single exemplar when summarizing.
    if opts.summary_only {
        for e in &mut entries {
            e.reasons.truncate(1);
        }
    }

    let mut forward_dependencies = Vec::new();
    let mut sibling_symbols = Vec::new();
    if opts.include_forward || opts.include_siblings {
        let target_files = impact_target_files(db, &req.target)?;
        if opts.include_forward {
            let mut fwd: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            for f in &target_files {
                // Storage call, not `lookups::list_dependencies`: only the
                // `imports` half is read here, and the wrapper's per-file
                // `imported_by` resolution sweep would run once per target
                // file for nothing.
                for imp in db.list_file_dependencies(f)?.imports {
                    fwd.insert(imp);
                }
            }
            for f in &target_files {
                fwd.remove(f);
            }
            forward_dependencies = fwd.into_iter().collect();
        }
        if opts.include_siblings {
            sibling_symbols = collect_sibling_symbols(db, &req.target, &target_files)?;
        }
    }

    Ok(ImpactReport {
        results: entries,
        forward_dependencies,
        sibling_symbols,
        truncated,
        summary,
    })
}

/// Resolve the file(s) the impact target lives in.
fn impact_target_files(db: &Database, target: &ImpactTarget) -> Result<Vec<String>> {
    match target {
        ImpactTarget::File { path } => Ok(vec![path.clone()]),
        ImpactTarget::Symbol { name } => {
            let mut files: Vec<String> = db
                .find_symbols(name, None)?
                .iter()
                .map(|s| s.file_path.clone())
                .collect();
            files.sort();
            files.dedup();
            Ok(files)
        }
    }
}

/// Symbols defined in the target's file(s), excluding the target symbol itself.
/// Repetitive same-name definitions (overloads) collapse to one entry, and the
/// list is capped at [`SIBLING_SYMBOL_CAP`].
fn collect_sibling_symbols(
    db: &Database,
    target: &ImpactTarget,
    target_files: &[String],
) -> Result<Vec<SiblingSymbol>> {
    let target_name = match target {
        ImpactTarget::Symbol { name } => Some(name.as_str()),
        ImpactTarget::File { .. } => None,
    };
    let mut seen_names: HashSet<String> = HashSet::new();
    let mut out: Vec<SiblingSymbol> = Vec::new();
    for f in target_files {
        for s in db.symbols_for_file(f)? {
            if let Some(n) = target_name
                && (s.name == n || s.qualified_name == n)
            {
                continue;
            }
            // Collapse repeated implementations (same name+kind) to one signature.
            let key = format!("{}::{}", s.kind.as_str(), s.name);
            if !seen_names.insert(key) {
                continue;
            }
            out.push(SiblingSymbol {
                name: s.name,
                kind: s.kind,
                line: s.line_start,
            });
            if out.len() >= SIBLING_SYMBOL_CAP {
                break;
            }
        }
        if out.len() >= SIBLING_SYMBOL_CAP {
            break;
        }
    }
    out.sort_by_key(|s| s.line);
    Ok(out)
}

fn build_impact_summary(entries: &[ImpactEntry]) -> ImpactSummary {
    let mut by_distance: HashMap<u32, usize> = HashMap::new();
    let (mut src, mut test, mut cfg) = (0usize, 0usize, 0usize);
    for e in entries {
        *by_distance.entry(e.distance).or_insert(0) += 1;
        match e.category {
            FileCategory::Source => src += 1,
            FileCategory::Test => test += 1,
            FileCategory::Config => cfg += 1,
        }
    }
    let mut by_distance: Vec<DistanceCount> = by_distance
        .into_iter()
        .map(|(distance, count)| DistanceCount { distance, count })
        .collect();
    by_distance.sort_by_key(|d| d.distance);
    let by_category: Vec<CategoryCount> = [
        (FileCategory::Source, src),
        (FileCategory::Test, test),
        (FileCategory::Config, cfg),
    ]
    .into_iter()
    .filter(|(_, count)| *count > 0)
    .map(|(category, count)| CategoryCount { category, count })
    .collect();
    ImpactSummary {
        total_affected: entries.len(),
        by_distance,
        by_category,
    }
}

pub(crate) fn references_for_symbol(db: &Database, sym: &Symbol) -> Result<Vec<Reference>> {
    // Look up by the SHORT name, never the qualified one. `find_references`
    // treats a qualified key as an exact `to_name` match, but a reference is
    // recorded under whatever spelling the source used: a PHP subclass in the
    // same namespace writes `extends Foo` with no `use`, so its row says `Foo`,
    // not `App\Foo`. Keying on the qualified name therefore matched only the
    // rows that happen to spell it out — in monolog, `Logger` kept the 15
    // `use Monolog\Logger` rows and dropped the 87 call/instantiation rows,
    // and `AbstractProcessingHandler` (30 subclasses, never imported because
    // they share its namespace) resolved to zero dependents.
    //
    // The short name goes through the `to_name_tail` branch, which matches both
    // spellings. Precision is not lost: the import-aware resolution below is
    // exactly the mechanism that narrows a broad tail match back down, and it
    // already had to handle this for symbols whose qualified name equals their
    // short name.
    let raw = db.find_references(&sym.name, None)?;

    // Import-aware reverse resolution. `find_references` matches by
    // `to_name_tail`, so an unqualified name fans out to *every* same-named
    // definition — a call to one class's `getAttributes` was counted toward
    // all of them, inflating the reverse blast radius that `impact_analysis`
    // and `assess_risk` read. `resolve_callee_definitions` already filters
    // candidates by the caller file's imports (the forward path); routing each
    // candidate reference back through it makes a reverse edge exist iff the
    // matching forward edge does. Unique names short-circuit in the resolver
    // (≤1 candidate), so distinctively-named symbols are untouched — only
    // genuinely ambiguous names get import-filtered. Resolutions are cached per
    // `(from_file, to_name)` because a hot symbol's callers repeat both.
    let mut out = Vec::with_capacity(raw.len());
    let mut cache: HashMap<(String, String), Vec<Symbol>> = HashMap::new();
    for r in raw {
        let cache_key = (r.from_file.clone(), r.to_name.clone());
        if !cache.contains_key(&cache_key) {
            let resolved = resolve_callee_definitions(db, &r.from_file, &r.to_name)?;
            cache.insert(cache_key.clone(), resolved);
        }
        if cache[&cache_key].iter().any(|s| same_symbol_def(s, sym)) {
            out.push(r);
        }
    }
    Ok(out)
}

/// Identity test for two `Symbol`s naming the same definition. `Symbol` carries
/// no stable id, so we key on the triple that uniquely locates a definition:
/// file, qualified name, and start line.
fn same_symbol_def(a: &Symbol, b: &Symbol) -> bool {
    a.file_path == b.file_path
        && a.qualified_name == b.qualified_name
        && a.line_start == b.line_start
}

fn symbol_identity_key(sym: &Symbol) -> (String, String, u32) {
    (
        sym.file_path.clone(),
        sym.qualified_name.clone(),
        sym.line_start,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One class with two callers, so the depth-1 pass leaves two symbols in
    /// the next frontier.
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

    fn file_request() -> ImpactRequest {
        ImpactRequest {
            target: ImpactTarget::File {
                path: "Repository.php".to_string(),
            },
            depth: 2,
            source_only: false,
        }
    }

    #[test]
    fn walk_reports_frontier_cap_and_keeps_shallow_entries() {
        let (_dir, db) = setup_project();
        let (entries, capped) = impact_analysis_walk(&db, &file_request(), 1).unwrap();
        assert!(
            capped,
            "two depth-1 callers with a frontier cap of 1 must report capped"
        );
        // Depth-1 entries are recorded before the frontier truncation, so the
        // capped walk still returns both direct dependents.
        let files: Vec<&str> = entries.iter().map(|e| e.file_path.as_str()).collect();
        assert!(files.contains(&"Controller.php"), "entries: {files:?}");
        assert!(files.contains(&"Service.php"), "entries: {files:?}");
    }

    #[test]
    fn walk_reports_no_cap_under_the_default_frontier() {
        let (_dir, db) = setup_project();
        let (entries, capped) = impact_analysis_walk(&db, &file_request(), MAX_FRONTIER).unwrap();
        assert!(!capped, "two callers must not trip the default cap");
        assert_eq!(entries.len(), 2, "entries: {entries:?}");
    }

    #[test]
    fn walk_on_file_with_no_symbols_is_empty_and_uncapped() {
        let (_dir, db) = setup_project();
        let req = ImpactRequest {
            target: ImpactTarget::File {
                path: "no-symbols.yaml".to_string(),
            },
            depth: 2,
            source_only: false,
        };
        let (entries, capped) = impact_analysis_walk(&db, &req, MAX_FRONTIER).unwrap();
        assert!(entries.is_empty());
        assert!(!capped);
    }
}
