use std::collections::{HashMap, HashSet};
use std::time::Instant;

use anyhow::Result;
use codesage_protocol::{
    CategoryCount, DistanceCount, FileCategory, ImpactEntry, ImpactOptions, ImpactReason,
    ImpactReport, ImpactRequest, ImpactSummary, ImpactTarget, Reference, ReferenceKind,
    SiblingSymbol, Symbol,
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
    let outcome = impact_analysis_walk_budgeted(db, req, max_frontier, None)?;
    Ok((outcome.entries, outcome.capped))
}

/// Result of [`impact_analysis_walk_budgeted`].
#[derive(Debug)]
pub(crate) struct WalkOutcome {
    pub entries: Vec<ImpactEntry>,
    /// Frontier cap, work budget, or deadline stopped the walk early; every
    /// derived count is a lower bound.
    pub capped: bool,
    /// Distinct resolved edges per dependent file, uncapped (an
    /// `ImpactEntry` keeps at most 10 `reasons`, so its length saturates on
    /// hub files and cannot rank them).
    pub edge_counts: HashMap<String, u32>,
    /// Symbols the walk started from. Zero with `capped: false` means the
    /// target is indexed but defines nothing, so an empty result is not a
    /// finding about its dependents.
    pub seed_count: usize,
}

/// Hard work budget for a walk, counted in resolution steps (see
/// [`WalkBudget::cost`]), plus an optional wall-clock deadline. One instance
/// serves several walks: [`WalkBudget::reset`] refills the steps between
/// inputs while the deadline and the per-name cost memo carry over.
#[derive(Debug)]
pub(crate) struct WalkBudget {
    pub remaining: usize,
    /// Steps spent or deadline passed; the current walk stops and later
    /// walks return immediately until `reset`.
    pub exhausted: bool,
    pub deadline: Option<Instant>,
    /// Sticky: once the deadline has passed, `reset` cannot revive a budget.
    pub deadline_hit: bool,
    candidate_counts: HashMap<String, usize>,
    caller_file_counts: HashMap<String, usize>,
}

impl WalkBudget {
    pub(crate) fn new(steps: usize, deadline: Option<Instant>) -> Self {
        Self {
            remaining: steps,
            exhausted: steps == 0,
            deadline,
            deadline_hit: false,
            candidate_counts: HashMap::new(),
            caller_file_counts: HashMap::new(),
        }
    }

    /// Refill the step budget for the next input. The deadline and the cost
    /// memo persist; a passed deadline stays exhausted.
    pub(crate) fn reset(&mut self, steps: usize) {
        self.remaining = steps;
        self.exhausted = steps == 0 || self.deadline_hit;
    }

    /// Charge `steps` for an admitted symbol. Spending the budget to exactly
    /// zero is not exhaustion: `exhausted` means something was skipped, and
    /// only the admission loop (or the deadline) can know that.
    fn charge(&mut self, steps: usize) {
        self.remaining = self.remaining.saturating_sub(steps);
    }

    /// `true` (and exhausted) once the wall-clock deadline has passed.
    pub(crate) fn over_deadline(&mut self) -> bool {
        if self.deadline_hit {
            return true;
        }
        if self.deadline.is_some_and(|d| Instant::now() >= d) {
            self.deadline_hit = true;
            self.exhausted = true;
            return true;
        }
        false
    }

    /// Predicted cost of resolving the references to `sym`: distinct caller
    /// files × candidate definitions sharing the short name.
    /// `resolve_callee_definitions` runs once per caller file and filters
    /// every candidate against that file's imports, so that product is the
    /// work, not the row count — on home-assistant, `__init__` is 4215 rows
    /// but 2970 files × 5927 candidates, and took 27 s where every other
    /// symbol in the file took under 40 ms. A unique name costs its
    /// caller-file count. Both counts are COUNT queries memoized per name, so
    /// pricing a symbol never hydrates a row.
    fn cost(&mut self, db: &Database, sym: &Symbol) -> Result<usize> {
        let files = match self.caller_file_counts.get(&sym.name) {
            Some(n) => *n,
            None => {
                let n = db.count_referencing_files(&sym.name)?;
                self.caller_file_counts.insert(sym.name.clone(), n);
                n
            }
        };
        let candidates = match self.candidate_counts.get(&sym.name) {
            Some(n) => *n,
            None => {
                let n = db.count_symbols_named(&sym.name)?.max(1);
                self.candidate_counts.insert(sym.name.clone(), n);
                n
            }
        };
        Ok(files.saturating_mul(candidates))
    }
}

/// [`impact_analysis_walk`] with an optional [`WalkBudget`]. Per level, every
/// unvisited frontier symbol is priced, the cheapest are admitted until the
/// budget runs out, and the admitted ones are then resolved in their original
/// frontier order — so a walk that skips nothing is identical to the
/// unbudgeted walk, and a hub name spends the budget instead of starving the
/// precise neighbours that came after it in file order. A symbol in a hot
/// file can have tens of thousands of reference rows, and a caller that only
/// needs "which tests reach this" must not pay for all of them.
pub(crate) fn impact_analysis_walk_budgeted(
    db: &Database,
    req: &ImpactRequest,
    max_frontier: usize,
    mut budget: Option<&mut WalkBudget>,
) -> Result<WalkOutcome> {
    if let Some(b) = budget.as_deref_mut()
        && (b.exhausted || b.over_deadline())
    {
        return Ok(WalkOutcome {
            entries: Vec::new(),
            capped: true,
            edge_counts: HashMap::new(),
            seed_count: 0,
        });
    }
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
        return Ok(WalkOutcome {
            entries: Vec::new(),
            capped: false,
            edge_counts: HashMap::new(),
            seed_count: 0,
        });
    }

    let origin_files: HashSet<String> = match &req.target {
        ImpactTarget::File { path } => {
            let mut s = HashSet::new();
            s.insert(path.clone());
            s
        }
        ImpactTarget::Symbol { .. } => seed_symbols.iter().map(|s| s.file_path.clone()).collect(),
    };

    // Per dependent file: shortest distance, the first 10 distinct reasons
    // (the wire projection), and the full set of distinct reasons keyed on
    // (via_symbol, kind, line) so the edge count neither caps at 10 nor
    // double-counts a reason that repeats after the tenth.
    type ReasonKey = (String, ReferenceKind, u32);
    let mut file_reasons: HashMap<String, (u32, Vec<ImpactReason>, HashSet<ReasonKey>)> =
        HashMap::new();
    let seed_count = seed_symbols.len();
    let mut frontier: Vec<Symbol> = seed_symbols;
    let mut visited_symbols: HashSet<(String, String, u32)> = HashSet::new();
    let mut frontier_capped = false;

    for depth in 1..=req.depth as u32 {
        // First pass: collect refs, update file_reasons, record (from_file, line) pairs
        // that need caller-symbol lookups for the next frontier.
        let mut pending_callers: Vec<(String, Option<String>, u32)> = Vec::new();
        let mut budget_spent = false;
        let mut level: Vec<(&Symbol, Vec<Reference>)> = Vec::new();
        if let Some(b) = budget.as_deref_mut() {
            // Admission is greedy cheapest-first over the priced level;
            // resolution then runs in frontier order over the admitted set
            // only, so the reason ordering (and therefore the output) matches
            // the unbudgeted walk whenever nothing is skipped.
            let mut priced: Vec<(usize, usize)> = Vec::new();
            for (idx, sym) in frontier.iter().enumerate() {
                // Pricing is two COUNT queries per symbol; on a wide level
                // that alone can outlast the deadline, so it is checked here
                // too: once at the start of every level and every 256 symbols.
                if idx % 256 == 0 && b.over_deadline() {
                    break;
                }
                if !visited_symbols.insert(symbol_identity_key(sym)) {
                    continue;
                }
                priced.push((b.cost(db, sym)?, idx));
            }
            priced.sort_unstable();
            let mut admitted = vec![false; frontier.len()];
            for (cost, idx) in priced {
                if cost > b.remaining {
                    b.exhausted = true;
                    break;
                }
                b.charge(cost);
                admitted[idx] = true;
            }
            for (idx, sym) in frontier.iter().enumerate() {
                if !admitted[idx] {
                    continue;
                }
                if b.over_deadline() {
                    break;
                }
                level.push((sym, references_for_symbol(db, sym)?));
            }
            budget_spent = b.exhausted;
        } else {
            for sym in &frontier {
                if !visited_symbols.insert(symbol_identity_key(sym)) {
                    continue;
                }
                level.push((sym, references_for_symbol(db, sym)?));
            }
        }
        for (sym, refs) in level {
            for r in refs {
                if origin_files.contains(&r.from_file) {
                    continue;
                }
                let entry = file_reasons
                    .entry(r.from_file.clone())
                    .or_insert_with(|| (depth, Vec::new(), HashSet::new()));
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
                let distinct =
                    entry
                        .2
                        .insert((reason.via_symbol.clone(), reason.kind, reason.line));
                if distinct && entry.1.len() < 10 {
                    entry.1.push(reason);
                }
                if depth < req.depth as u32 {
                    pending_callers.push((r.from_file, r.from_symbol, r.line));
                }
            }
        }

        if budget_spent {
            // Rows fetched so far are recorded; the next level would need more
            // fetches, so everything deeper is unknown.
            frontier_capped = true;
            break;
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
        // The caller lookup for a level is unpriced: it hydrates every symbol
        // of every file the level reached. Check the clock once before it.
        if let Some(b) = budget.as_deref_mut()
            && b.over_deadline()
        {
            frontier_capped = true;
            break;
        }
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

    let edge_counts: HashMap<String, u32> = file_reasons
        .iter()
        .map(|(path, (_, _, edges))| (path.clone(), edges.len() as u32))
        .collect();
    let mut entries: Vec<ImpactEntry> = file_reasons
        .into_iter()
        .map(|(path, (distance, reasons, _))| {
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
    Ok(WalkOutcome {
        entries,
        capped: frontier_capped,
        edge_counts,
        seed_count,
    })
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
        counts_floor: true,
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
    resolve_references_to_symbol(db, sym, raw)
}

/// Second half of [`references_for_symbol`]: keep only the raw rows whose
/// callsite actually resolves to `sym`.
fn resolve_references_to_symbol(
    db: &Database,
    sym: &Symbol,
    raw: Vec<Reference>,
) -> Result<Vec<Reference>> {
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
    fn report_discloses_that_counts_are_a_floor() {
        let (_dir, db) = setup_project();
        let report =
            impact_analysis_report(&db, &file_request(), &ImpactOptions::default()).unwrap();
        assert!(report.counts_floor, "report: {report:?}");
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["counts_floor"], serde_json::Value::Bool(true));
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

    #[test]
    fn budgeted_walk_that_skips_nothing_matches_the_unbudgeted_walk() {
        let (_dir, db) = setup_project();
        let (plain, plain_capped) =
            impact_analysis_walk(&db, &file_request(), MAX_FRONTIER).unwrap();
        let mut budget = WalkBudget::new(usize::MAX, None);
        let out =
            impact_analysis_walk_budgeted(&db, &file_request(), MAX_FRONTIER, Some(&mut budget))
                .unwrap();
        assert!(!plain_capped);
        assert!(!out.capped, "unlimited budget must not cap");
        assert_eq!(format!("{plain:?}"), format!("{:?}", out.entries));
        assert_eq!(out.edge_counts.len(), plain.len());
        for entry in &plain {
            assert_eq!(
                out.edge_counts[&entry.file_path] as usize,
                entry.reasons.len(),
                "under the reason cap the uncapped edge count equals reasons.len()"
            );
        }
    }

    #[test]
    fn budgeted_walk_reports_deadline_and_zero_budget_as_capped() {
        let (_dir, db) = setup_project();
        let mut spent = WalkBudget::new(0, None);
        let out =
            impact_analysis_walk_budgeted(&db, &file_request(), MAX_FRONTIER, Some(&mut spent))
                .unwrap();
        assert!(out.capped);
        assert!(out.entries.is_empty());

        let mut late = WalkBudget::new(usize::MAX, Some(Instant::now()));
        let out =
            impact_analysis_walk_budgeted(&db, &file_request(), MAX_FRONTIER, Some(&mut late))
                .unwrap();
        assert!(out.capped);
        assert!(late.deadline_hit);
        late.reset(usize::MAX);
        assert!(late.exhausted, "a passed deadline survives reset");
    }

    /// Repository.php defines `find` (three callers) and `save` (one caller):
    /// the seed symbols price differently, so cheapest-first admission visits
    /// `save` before `find` while frontier order is `find`, `save`.
    fn setup_project_with_uneven_costs() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("Repository.php"),
            b"<?php\nnamespace App;\nclass Repository {\n  public function find($id) { return null; }\n  public function save($e) { return true; }\n}\n",
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
        std::fs::write(
            root.join("Job.php"),
            b"<?php\nnamespace App;\nuse App\\Repository;\nclass Job {\n  public function go(Repository $r) { $r->save(1); return $r->find(2); }\n}\n",
        )
        .unwrap();
        let db = Database::open_in_memory().unwrap();
        crate::full_index(root, &db, &[], false).unwrap();
        (dir, db)
    }

    #[test]
    fn budgeted_walk_with_uneven_costs_still_matches_the_unbudgeted_walk() {
        let (_dir, db) = setup_project_with_uneven_costs();
        let mut probe = WalkBudget::new(usize::MAX, None);
        let find = db
            .find_symbols("find", None)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let save = db
            .find_symbols("save", None)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        assert!(
            probe.cost(&db, &save).unwrap() < probe.cost(&db, &find).unwrap(),
            "fixture must price the symbols unevenly for this test to mean anything"
        );

        let (plain, plain_capped) =
            impact_analysis_walk(&db, &file_request(), MAX_FRONTIER).unwrap();
        let mut budget = WalkBudget::new(usize::MAX, None);
        let out =
            impact_analysis_walk_budgeted(&db, &file_request(), MAX_FRONTIER, Some(&mut budget))
                .unwrap();
        assert!(!plain_capped && !out.capped);
        assert_eq!(plain.len(), 3, "entries: {plain:?}");
        assert_eq!(format!("{plain:?}"), format!("{:?}", out.entries));
    }

    #[test]
    fn edge_count_dedupes_reasons_beyond_the_ten_kept() {
        use codesage_protocol::{FileInfo, Language, ReferenceKind, SymbolKind};

        let db = Database::open_in_memory().unwrap();
        let repo = db
            .upsert_file(&FileInfo {
                path: "Repository.php".to_string(),
                language: Language::Php,
                content_hash: "r".to_string(),
            })
            .unwrap();
        let names: Vec<String> = (1..=15).map(|i| format!("op{i:02}")).collect();
        let syms: Vec<Symbol> = names
            .iter()
            .map(|n| Symbol {
                name: n.clone(),
                qualified_name: n.clone(),
                kind: SymbolKind::Function,
                file_path: "Repository.php".to_string(),
                line_start: 1,
                line_end: 10,
                col_start: 0,
                col_end: 0,
                rationale: vec![],
            })
            .collect();
        db.insert_symbols(repo, &syms).unwrap();

        let caller = db
            .upsert_file(&FileInfo {
                path: "Caller.php".to_string(),
                language: Language::Php,
                content_hash: "c".to_string(),
            })
            .unwrap();
        let mut refs: Vec<Reference> = names
            .iter()
            .enumerate()
            .map(|(i, n)| Reference {
                from_file: "Caller.php".to_string(),
                from_symbol: None,
                to_name: n.clone(),
                kind: ReferenceKind::Call,
                line: 10 + i as u32,
                col: 0,
            })
            .collect();
        // Five repeats of already-recorded (symbol, kind, line) reasons at a
        // different column: distinct rows, the same edge.
        for (i, n) in names.iter().enumerate().take(5) {
            refs.push(Reference {
                from_file: "Caller.php".to_string(),
                from_symbol: None,
                to_name: n.clone(),
                kind: ReferenceKind::Call,
                line: 10 + i as u32,
                col: 8,
            });
        }
        db.insert_references(caller, &refs).unwrap();

        let out = impact_analysis_walk_budgeted(&db, &file_request(), MAX_FRONTIER, None).unwrap();
        assert_eq!(out.entries.len(), 1);
        assert_eq!(out.entries[0].file_path, "Caller.php");
        assert_eq!(
            out.entries[0].reasons.len(),
            10,
            "wire projection stays capped"
        );
        assert_eq!(out.edge_counts["Caller.php"], 15, "15 distinct, 5 repeats");
    }
}
