use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use codesage_protocol::{
    CategoryCount, DistanceCount, FileCategory, ImpactEntry, ImpactOptions, ImpactReason,
    ImpactReport, ImpactRequest, ImpactSummary, ImpactTarget, Reference, ReferenceKind,
    SiblingSymbol, Symbol,
};
use codesage_storage::Database;

use crate::bundle::{
    import_ref_targets_file, import_refs_for_file, resolve_callee_definitions,
    resolve_callee_definitions_with_imports,
};

pub(crate) fn is_qualified_symbol_name(name: &str) -> bool {
    name.contains('\\') || name.contains('.') || name.contains("::")
}

/// Bound per-level fan-out; capped walks report counts as lower bounds.
pub(crate) const MAX_FRONTIER: usize = 512;

pub fn impact_analysis(db: &Database, req: &ImpactRequest) -> Result<Vec<ImpactEntry>> {
    Ok(impact_analysis_walk(db, req, MAX_FRONTIER)?.0)
}

/// [`impact_analysis`] plus a `frontier_capped` flag: `true` when any level's
/// frontier was truncated at `max_frontier`, meaning entries at deeper
/// distances may be missing and every derived count is a lower bound.
pub(crate) fn impact_analysis_walk(
    db: &Database,
    req: &ImpactRequest,
    max_frontier: usize,
) -> Result<(Vec<ImpactEntry>, bool)> {
    let outcome = impact_analysis_walk_budgeted(db, req, max_frontier, None)?;
    Ok((outcome.entries, outcome.capped))
}

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
    /// Symbol seeds, or one for a file-only root with a resolved import edge.
    pub seed_count: usize,
}

type DefinitionKey = (String, String, u32);
type ResolvedDefinitions = Arc<Vec<DefinitionKey>>;

/// Request-local resolved edges shared across walks with different frontier
/// and work limits. Admission is still charged on cache hits.
#[derive(Default)]
pub(crate) struct WalkCache {
    references: HashMap<(String, String, u32), Arc<Vec<Reference>>>,
    resolutions: HashMap<(String, String), ResolvedDefinitions>,
    caller_imports: HashMap<String, Arc<Vec<String>>>,
    reference_bytes: usize,
    file_imports: Option<Arc<Vec<Reference>>>,
    import_matches: HashMap<String, Arc<Vec<usize>>>,
    #[cfg(test)]
    hits: usize,
    #[cfg(test)]
    resolution_hits: usize,
    #[cfg(test)]
    resolution_loads: usize,
    #[cfg(test)]
    caller_import_loads: usize,
}

impl WalkCache {
    const MAX_SYMBOLS: usize = 8192;
    const MAX_REFERENCE_BYTES: usize = 16 * 1024 * 1024;

    #[cfg(test)]
    pub(crate) fn stats(&self) -> (usize, usize, usize) {
        (self.hits, self.entry_count(), self.reference_bytes)
    }

    fn references(&mut self, db: &Database, sym: &Symbol) -> Result<Arc<Vec<Reference>>> {
        codesage_protocol::work::checkpoint()?;
        let key = symbol_identity_key(sym);
        if let Some(rows) = self.references.get(&key) {
            #[cfg(test)]
            {
                self.hits += 1;
            }
            return Ok(Arc::clone(rows));
        }
        let raw = db.find_references(&sym.name, None)?;
        let rows = Arc::new(resolve_references_to_symbol(db, sym, raw, Some(self))?);
        let bytes = rows.iter().fold(
            rows.capacity()
                .saturating_mul(std::mem::size_of::<Reference>())
                .saturating_add(key.0.capacity())
                .saturating_add(key.1.capacity()),
            |bytes, row| {
                bytes
                    .saturating_add(row.from_file.capacity())
                    .saturating_add(row.to_name.capacity())
                    .saturating_add(row.from_symbol.as_ref().map_or(0, String::capacity))
            },
        );
        if self.entry_count() < Self::MAX_SYMBOLS
            && bytes <= Self::MAX_REFERENCE_BYTES.saturating_sub(self.reference_bytes)
        {
            self.reference_bytes += bytes;
            self.references.insert(key, Arc::clone(&rows));
        }
        Ok(rows)
    }

    fn entry_count(&self) -> usize {
        self.references.len()
            + self.import_matches.len()
            + self.resolutions.len()
            + self.caller_imports.len()
    }

    fn resolve(
        &mut self,
        db: &Database,
        caller_file: &str,
        spelling: &str,
    ) -> Result<ResolvedDefinitions> {
        codesage_protocol::work::checkpoint()?;
        let key = (caller_file.to_string(), spelling.to_string());
        if let Some(rows) = self.resolutions.get(&key) {
            #[cfg(test)]
            {
                self.resolution_hits += 1;
            }
            return Ok(Arc::clone(rows));
        }
        #[cfg(test)]
        {
            self.resolution_loads += 1;
        }
        let rows = definition_keys(resolve_callee_definitions_with_imports(
            db,
            caller_file,
            spelling,
            &mut || self.caller_imports(db, caller_file),
        )?);
        let bytes = rows.iter().fold(
            rows.capacity()
                .saturating_mul(std::mem::size_of::<DefinitionKey>())
                .saturating_add(key.0.capacity())
                .saturating_add(key.1.capacity()),
            |bytes, row| {
                bytes
                    .saturating_add(row.0.capacity())
                    .saturating_add(row.1.capacity())
            },
        );
        if self.entry_count() < Self::MAX_SYMBOLS
            && bytes <= Self::MAX_REFERENCE_BYTES.saturating_sub(self.reference_bytes)
        {
            self.reference_bytes += bytes;
            self.resolutions.insert(key, Arc::clone(&rows));
        }
        Ok(rows)
    }

    fn caller_imports(&mut self, db: &Database, caller_file: &str) -> Result<Arc<Vec<String>>> {
        codesage_protocol::work::checkpoint()?;
        if let Some(imports) = self.caller_imports.get(caller_file) {
            return Ok(Arc::clone(imports));
        }
        #[cfg(test)]
        {
            self.caller_import_loads += 1;
        }
        let imports = Arc::new(import_refs_for_file(db, caller_file)?);
        let key = caller_file.to_string();
        let bytes = imports.iter().fold(
            imports
                .capacity()
                .saturating_mul(std::mem::size_of::<String>())
                .saturating_add(key.capacity()),
            |bytes, import| bytes.saturating_add(import.capacity()),
        );
        if self.entry_count() < Self::MAX_SYMBOLS
            && bytes <= Self::MAX_REFERENCE_BYTES.saturating_sub(self.reference_bytes)
        {
            self.reference_bytes += bytes;
            self.caller_imports.insert(key, Arc::clone(&imports));
        }
        Ok(imports)
    }

    fn remember_imports(&mut self, file: &str, matches: Vec<usize>) {
        let bytes = matches
            .capacity()
            .saturating_mul(std::mem::size_of::<usize>())
            .saturating_add(file.len());
        if self.entry_count() < Self::MAX_SYMBOLS
            && bytes <= Self::MAX_REFERENCE_BYTES.saturating_sub(self.reference_bytes)
        {
            self.reference_bytes += bytes;
            self.import_matches
                .insert(file.to_string(), Arc::new(matches));
        }
    }
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
    file_imports: Option<Arc<Vec<Reference>>>,
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
            file_imports: None,
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

    /// Price resolution as caller files × same-name definitions, not reference rows.
    /// Memoized COUNT queries avoid hydrating candidates just to price them.
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

/// Admit cheaper symbols first within the budget, then resolve in frontier order.
/// Unrestricted walks retain their original result ordering.
pub(crate) fn impact_analysis_walk_budgeted(
    db: &Database,
    req: &ImpactRequest,
    max_frontier: usize,
    budget: Option<&mut WalkBudget>,
) -> Result<WalkOutcome> {
    impact_analysis_walk_shared(db, req, max_frontier, budget, None)
}

pub(crate) fn impact_analysis_walk_shared(
    db: &Database,
    req: &ImpactRequest,
    max_frontier: usize,
    mut budget: Option<&mut WalkBudget>,
    mut cache: Option<&mut WalkCache>,
) -> Result<WalkOutcome> {
    codesage_protocol::work::checkpoint()?;
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
                // Identical qualified names cannot disambiguate definitions;
                // retain their union instead of suggesting an unusable name.
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

    let mut file_frontier = match &req.target {
        ImpactTarget::File { path } if db.file_id_for_path(path)?.is_some() => vec![path.clone()],
        _ => Vec::new(),
    };
    if seed_symbols.is_empty() && file_frontier.is_empty() {
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
    let mut seed_count = seed_symbols.len();
    let file_imports = if file_frontier.is_empty() {
        Arc::new(Vec::new())
    } else if let Some(cache) = cache.as_deref_mut() {
        if cache.file_imports.is_none() {
            cache.file_imports = Some(Arc::new(db.file_import_references()?));
        }
        Arc::clone(
            cache
                .file_imports
                .as_ref()
                .expect("file imports initialized"),
        )
    } else if let Some(b) = budget.as_deref_mut() {
        if b.file_imports.is_none() {
            b.file_imports = Some(Arc::new(db.file_import_references()?));
        }
        Arc::clone(b.file_imports.as_ref().expect("file imports initialized"))
    } else {
        Arc::new(db.file_import_references()?)
    };
    let mut visited_files = HashSet::new();
    let mut frontier: Vec<Symbol> = seed_symbols;
    let mut visited_symbols: HashSet<(String, String, u32)> = HashSet::new();
    let mut frontier_capped = false;

    for depth in 1..=req.depth as u32 {
        codesage_protocol::work::checkpoint()?;
        let mut next_files = Vec::new();
        let mut pending_callers: Vec<(String, Option<String>, u32)> = Vec::new();
        let mut budget_spent = false;
        let mut level: Vec<(&Symbol, Arc<Vec<Reference>>)> = Vec::new();
        if let Some(b) = budget.as_deref_mut() {
            let mut priced: Vec<(usize, usize)> = Vec::new();
            for (idx, sym) in frontier.iter().enumerate() {
                codesage_protocol::work::checkpoint()?;
                // Pricing alone can exhaust the deadline on wide frontiers.
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
                codesage_protocol::work::checkpoint()?;
                if cost > b.remaining {
                    b.exhausted = true;
                    break;
                }
                b.charge(cost);
                admitted[idx] = true;
            }
            for (idx, sym) in frontier.iter().enumerate() {
                if !admitted[idx] {
                    codesage_protocol::work::checkpoint()?;
                    continue;
                }
                if b.over_deadline() {
                    break;
                }
                let rows = match cache.as_deref_mut() {
                    Some(cache) => cache.references(db, sym)?,
                    None => Arc::new(references_for_symbol(db, sym)?),
                };
                level.push((sym, rows));
            }
            budget_spent = b.exhausted;
        } else {
            for sym in &frontier {
                codesage_protocol::work::checkpoint()?;
                if !visited_symbols.insert(symbol_identity_key(sym)) {
                    continue;
                }
                let rows = match cache.as_deref_mut() {
                    Some(cache) => cache.references(db, sym)?,
                    None => Arc::new(references_for_symbol(db, sym)?),
                };
                level.push((sym, rows));
            }
        }
        for (sym, refs) in level {
            codesage_protocol::work::checkpoint()?;
            for r in refs.iter() {
                codesage_protocol::work::checkpoint()?;
                if origin_files.contains(&r.from_file) {
                    continue;
                }
                let entry = file_reasons
                    .entry(r.from_file.clone())
                    .or_insert_with(|| (depth, Vec::new(), HashSet::new()));
                if entry.0 > depth {
                    entry.0 = depth;
                }
                // Same-named seeds can repeat a row; duplicates must not inflate ranking.
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
                    if matches!(req.target, ImpactTarget::File { .. }) {
                        next_files.push(r.from_file.clone());
                    }
                    pending_callers.push((r.from_file.clone(), r.from_symbol.clone(), r.line));
                }
            }
        }

        for file in &file_frontier {
            codesage_protocol::work::checkpoint()?;
            if !visited_files.insert(file.clone()) {
                continue;
            }
            if let Some(b) = budget.as_deref_mut() {
                if b.exhausted || b.over_deadline() || file_imports.len() > b.remaining {
                    b.exhausted = true;
                    budget_spent = true;
                    break;
                }
                b.charge(file_imports.len());
            }
            let cached_matches = cache
                .as_deref()
                .and_then(|cache| cache.import_matches.get(file))
                .cloned();
            let mut matches = Vec::new();
            for index in 0..cached_matches
                .as_ref()
                .map_or(file_imports.len(), |rows| rows.len())
            {
                codesage_protocol::work::checkpoint()?;
                if index % 256 == 0
                    && let Some(b) = budget.as_deref_mut()
                    && b.over_deadline()
                {
                    budget_spent = true;
                    break;
                }
                let reference_index = cached_matches.as_ref().map_or(index, |rows| rows[index]);
                let r = &file_imports[reference_index];
                if cached_matches.is_none()
                    && !import_ref_targets_file(&r.to_name, &r.from_file, file)
                {
                    continue;
                }
                if cache.is_some() && cached_matches.is_none() {
                    matches.push(reference_index);
                }
                if origin_files.contains(&r.from_file) {
                    continue;
                }
                seed_count = seed_count.max(1);
                let entry = file_reasons
                    .entry(r.from_file.clone())
                    .or_insert_with(|| (depth, Vec::new(), HashSet::new()));
                entry.0 = entry.0.min(depth);
                if entry.2.insert((r.to_name.clone(), r.kind, r.line)) && entry.1.len() < 10 {
                    entry.1.push(ImpactReason {
                        via_symbol: r.to_name.clone(),
                        kind: r.kind,
                        line: r.line,
                    });
                }
                if depth < req.depth as u32 {
                    next_files.push(r.from_file.clone());
                    pending_callers.push((r.from_file.clone(), r.from_symbol.clone(), r.line));
                }
            }
            if !budget_spent
                && cached_matches.is_none()
                && let Some(cache) = cache.as_deref_mut()
            {
                cache.remember_imports(file, matches);
            }
        }

        if budget_spent {
            frontier_capped = true;
            break;
        }
        if pending_callers.is_empty() && next_files.is_empty() {
            break;
        }

        let distinct_files: Vec<String> = {
            let mut set: HashSet<String> = HashSet::new();
            pending_callers.iter().for_each(|(f, _, _)| {
                set.insert(f.clone());
            });
            set.into_iter().collect()
        };
        // Caller hydration is not included in the step price.
        if let Some(b) = budget.as_deref_mut()
            && b.over_deadline()
        {
            frontier_capped = true;
            break;
        }
        let syms_by_file = db.symbols_for_files(&distinct_files)?;

        let mut next_frontier: Vec<Symbol> = Vec::new();
        for (from_file, from_symbol, line) in &pending_callers {
            codesage_protocol::work::checkpoint()?;
            let Some(syms) = syms_by_file.get(from_file) else {
                continue;
            };
            // Prefer the recorded owner over enclosing classes or functions.
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
                codesage_protocol::work::checkpoint()?;
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

        next_files.sort();
        next_files.dedup();
        next_files.retain(|file| !visited_files.contains(file));
        if next_files.len() > max_frontier {
            next_files.truncate(max_frontier);
            frontier_capped = true;
        }
        if next_frontier.is_empty() && next_files.is_empty() {
            break;
        }
        frontier = next_frontier;
        file_frontier = next_files;
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

    // Break ties before callers truncate, keeping identical queries deterministic.
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
                codesage_protocol::work::checkpoint()?;
                // Avoid the wrapper's imported_by resolution; only imports are needed.
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
        codesage_protocol::work::checkpoint()?;
        for s in db.symbols_for_file(f)? {
            codesage_protocol::work::checkpoint()?;
            if let Some(n) = target_name
                && (s.name == n || s.qualified_name == n)
            {
                continue;
            }
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
    // Source references may use a bare spelling even for qualified definitions.
    // Tail lookup admits both; import-aware resolution filters candidates below.
    let raw = db.find_references(&sym.name, None)?;
    resolve_references_to_symbol(db, sym, raw, None)
}

/// Second half of [`references_for_symbol`]: keep only the raw rows whose
/// callsite actually resolves to `sym`.
fn resolve_references_to_symbol(
    db: &Database,
    sym: &Symbol,
    raw: Vec<Reference>,
    mut shared: Option<&mut WalkCache>,
) -> Result<Vec<Reference>> {
    // A reverse edge must agree with forward import-aware resolution.
    // Cache repeated (caller file, spelling) lookups.
    let mut out = Vec::with_capacity(raw.len());
    let mut cache = HashMap::new();
    let identity = symbol_identity_key(sym);
    for r in raw {
        codesage_protocol::work::checkpoint()?;
        let cache_key = (r.from_file.clone(), r.to_name.clone());
        if !cache.contains_key(&cache_key) {
            let resolved = match shared.as_deref_mut() {
                Some(shared) => shared.resolve(db, &r.from_file, &r.to_name)?,
                None => resolved_definition_keys(db, &r.from_file, &r.to_name)?,
            };
            cache.insert(cache_key.clone(), resolved);
        }
        if cache[&cache_key].contains(&identity) {
            out.push(r);
        }
    }
    Ok(out)
}

fn resolved_definition_keys(
    db: &Database,
    caller_file: &str,
    spelling: &str,
) -> Result<ResolvedDefinitions> {
    Ok(definition_keys(resolve_callee_definitions(
        db,
        caller_file,
        spelling,
    )?))
}

fn definition_keys(symbols: Vec<Symbol>) -> ResolvedDefinitions {
    Arc::new(
        symbols
            .into_iter()
            .map(|sym| (sym.file_path, sym.qualified_name, sym.line_start))
            .collect(),
    )
}

/// Symbols have no stable id; this triple identifies one definition.
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

    fn setup_resolution_project() -> (Database, Vec<Symbol>) {
        let db = Database::open_in_memory().unwrap();
        db.execute_raw_for_tests(
            "INSERT INTO files (id, path, language, content_hash) VALUES
                (1, 'base.ts', 'typescript', 'base'),
                (2, 'other.ts', 'typescript', 'other'),
                (3, 'caller.ts', 'typescript', 'caller'),
                (4, 'orphan.ts', 'typescript', 'orphan');
             INSERT INTO symbols
                (file_id, name, qualified_name, kind, line_start, line_end, col_start, col_end)
                VALUES (1, 'anchor', 'anchor', 'function', 1, 1, 0, 20),
                       (2, 'anchor', 'anchor', 'function', 1, 1, 0, 20);
             INSERT INTO refs (from_file_id, to_name, to_name_tail, kind, line, col) VALUES
                (3, './base', 'base', 'import', 1, 0),
                (3, 'anchor', 'anchor', 'call', 2, 0),
                (3, 'anchor', 'anchor', 'call', 3, 0),
                (4, 'anchor', 'anchor', 'call', 1, 0),
                (4, 'anchor', 'anchor', 'call', 2, 0);",
        )
        .unwrap();
        let mut symbols = db.find_symbols("anchor", None).unwrap();
        symbols.sort_by(|a, b| a.file_path.cmp(&b.file_path));
        assert_eq!(symbols.len(), 2);
        (db, symbols)
    }

    fn assert_same_references(actual: &[Reference], expected: &[Reference]) {
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }

    fn setup_caller_import_project() -> Database {
        let (db, _) = setup_resolution_project();
        db.execute_raw_for_tests(
            "INSERT INTO symbols
                (file_id, name, qualified_name, kind, line_start, line_end, col_start, col_end)
                VALUES (1, 'second', 'second', 'function', 2, 2, 0, 20),
                       (2, 'second', 'second', 'function', 2, 2, 0, 20),
                       (1, 'unique', 'Scope::unique', 'function', 3, 3, 0, 20);
             INSERT INTO refs (from_file_id, to_name, to_name_tail, kind, line, col)
                VALUES (3, './base', 'base', 'import', 4, 0);",
        )
        .unwrap();
        db
    }

    #[test]
    fn caller_imports_reuse_decoded_evidence_across_spellings_without_crossing_callers() {
        let db = setup_caller_import_project();
        let mut cache = WalkCache::default();
        for (caller, expected_file) in [("caller.ts", Some("base.ts")), ("orphan.ts", None)] {
            for spelling in ["anchor", "second"] {
                let actual = cache.resolve(&db, caller, spelling).unwrap();
                assert_eq!(
                    actual,
                    resolved_definition_keys(&db, caller, spelling).unwrap()
                );
                assert_eq!(actual.first().map(|row| row.0.as_str()), expected_file);
                assert_eq!(actual.len(), usize::from(expected_file.is_some()));
            }
        }
        assert_eq!(cache.caller_import_loads, 2);
        assert_eq!(cache.caller_imports["caller.ts"].as_slice(), ["./base"]);
        assert!(cache.caller_imports["orphan.ts"].is_empty());
        assert_eq!(cache.entry_count(), 6);
        assert!(cache.reference_bytes <= WalkCache::MAX_REFERENCE_BYTES);
    }

    #[test]
    fn caller_imports_stay_lazy_for_unique_and_qualified_resolution() {
        let db = setup_caller_import_project();
        db.execute_raw_for_tests(
            "INSERT INTO refs (from_file_id, to_name, to_name_tail, kind, line, col)
             VALUES (3, 'unrelated_noise', 'unrelated_noise', 'invalid_resolution_kind', 5, 0);",
        )
        .unwrap();
        let mut cache = WalkCache::default();
        for spelling in ["unique", "Scope::unique", "scope::UNIQUE", "missing"] {
            assert_eq!(
                cache.resolve(&db, "caller.ts", spelling).unwrap(),
                resolved_definition_keys(&db, "caller.ts", spelling).unwrap()
            );
        }
        assert_eq!(cache.caller_import_loads, 0);
        assert!(cache.caller_imports.is_empty());
        let error = cache.resolve(&db, "caller.ts", "anchor").unwrap_err();
        assert!(format!("{error:#}").contains("invalid_resolution_kind"));
    }

    #[test]
    fn caller_imports_share_entry_and_byte_limits_with_other_walk_evidence() {
        let db = setup_caller_import_project();
        for byte_full in [false, true] {
            let mut cache = WalkCache::default();
            if byte_full {
                cache.reference_bytes = WalkCache::MAX_REFERENCE_BYTES;
            } else {
                for index in 0..WalkCache::MAX_SYMBOLS {
                    cache
                        .import_matches
                        .insert(index.to_string(), Arc::new(Vec::new()));
                }
            }
            let retained = (cache.entry_count(), cache.reference_bytes);
            for spelling in ["anchor", "second"] {
                assert_eq!(
                    cache.resolve(&db, "caller.ts", spelling).unwrap(),
                    resolved_definition_keys(&db, "caller.ts", spelling).unwrap()
                );
            }
            assert_eq!(cache.caller_import_loads, 2);
            assert!(cache.caller_imports.is_empty());
            assert_eq!((cache.entry_count(), cache.reference_bytes), retained);
        }
    }

    #[test]
    fn retained_caller_imports_charge_capacities_before_resolutions_are_admitted() {
        let db = setup_caller_import_project();
        let mut cache = WalkCache::default();
        for index in 0..WalkCache::MAX_SYMBOLS - 1 {
            cache
                .import_matches
                .insert(index.to_string(), Arc::new(Vec::new()));
        }
        let rows = cache.resolve(&db, "caller.ts", "anchor").unwrap();
        assert_eq!(rows[0].0, "base.ts");
        assert!(cache.resolutions.is_empty());
        assert_eq!(cache.entry_count(), WalkCache::MAX_SYMBOLS);
        let (key, imports) = cache.caller_imports.get_key_value("caller.ts").unwrap();
        assert_eq!(
            cache.reference_bytes,
            key.capacity()
                + imports.capacity() * std::mem::size_of::<String>()
                + imports.iter().map(String::capacity).sum::<usize>()
        );
    }

    #[test]
    fn oversized_caller_import_evidence_falls_back_without_poisoning_small_entries() {
        let db = setup_caller_import_project();
        db.execute_raw_for_tests(&format!(
            "INSERT INTO refs (from_file_id, to_name, to_name_tail, kind, line, col)
             VALUES (3, printf('%.*c', {}, 'x'), 'oversized', 'import', 5, 0)",
            WalkCache::MAX_REFERENCE_BYTES + 1
        ))
        .unwrap();
        let mut cache = WalkCache::default();
        for spelling in ["anchor", "second"] {
            let actual = cache.resolve(&db, "caller.ts", spelling).unwrap();
            assert_eq!(
                actual,
                resolved_definition_keys(&db, "caller.ts", spelling).unwrap()
            );
            assert_eq!(actual[0].0, "base.ts");
        }
        assert_eq!(cache.caller_import_loads, 2);
        assert!(cache.caller_imports.is_empty());
        assert_eq!(cache.resolutions.len(), 2);
        assert!(cache.reference_bytes < WalkCache::MAX_REFERENCE_BYTES);
        assert!(cache.caller_imports(&db, "orphan.ts").unwrap().is_empty());
        assert!(cache.caller_imports.contains_key("orphan.ts"));
    }

    #[test]
    fn caller_import_errors_do_not_cache_partial_evidence_or_block_retry() {
        let db = setup_caller_import_project();
        db.execute_raw_for_tests(
            "INSERT INTO refs (from_file_id, to_name, to_name_tail, kind, line, col)
             VALUES (3, 'unrelated_noise', 'unrelated_noise', 'invalid_resolution_kind', 5, 0);",
        )
        .unwrap();
        let mut cache = WalkCache::default();
        let actual = cache.resolve(&db, "caller.ts", "anchor").unwrap_err();
        let expected = resolved_definition_keys(&db, "caller.ts", "anchor").unwrap_err();
        assert_eq!(format!("{actual:#}"), format!("{expected:#}"));
        assert!(format!("{actual:#}").contains("invalid_resolution_kind"));
        assert!(cache.caller_imports.is_empty());
        assert!(cache.resolutions.is_empty());
        assert_eq!(cache.reference_bytes, 0);
        db.execute_raw_for_tests("DELETE FROM refs WHERE to_name = 'unrelated_noise'")
            .unwrap();
        for spelling in ["anchor", "second"] {
            let actual = cache.resolve(&db, "caller.ts", spelling).unwrap();
            assert_eq!(
                actual,
                resolved_definition_keys(&db, "caller.ts", spelling).unwrap()
            );
            assert_eq!(actual[0].0, "base.ts");
        }
        assert_eq!(cache.caller_import_loads, 2);
    }

    #[test]
    fn cancelled_work_cannot_reuse_warm_caller_import_evidence() {
        use codesage_protocol::work::{StopReason, WorkControl, WorkStopped};

        let db = setup_caller_import_project();
        let mut cache = WalkCache::default();
        assert_eq!(
            cache.resolve(&db, "caller.ts", "anchor").unwrap()[0].0,
            "base.ts"
        );
        let control = WorkControl::new(None);
        control.cancel(StopReason::ClientCancelled);
        let _scope = control.enter();
        let error = cache.caller_imports(&db, "caller.ts").unwrap_err();
        assert_eq!(
            error.downcast_ref::<WorkStopped>().unwrap().reason,
            StopReason::ClientCancelled
        );
        assert_eq!(cache.caller_import_loads, 1);
    }

    #[test]
    fn shared_resolutions_reuse_positive_and_empty_membership_across_symbols() {
        let (db, symbols) = setup_resolution_project();
        let mut cache = WalkCache::default();
        for (sym, expected_count) in symbols.iter().zip([2, 0]) {
            let expected = references_for_symbol(&db, sym).unwrap();
            assert_eq!(expected.len(), expected_count);
            let actual = cache.references(&db, sym).unwrap();
            assert_same_references(&actual, &expected);
        }
        assert_eq!(cache.resolution_loads, 2);
        assert_eq!(cache.resolution_hits, 2);
        assert_eq!(cache.resolutions.len(), 2);
        assert!(cache.resolutions[&("orphan.ts".into(), "anchor".into())].is_empty());
        assert!(cache.reference_bytes <= WalkCache::MAX_REFERENCE_BYTES);

        let mut different_line = symbols[0].clone();
        different_line.line_start += 1;
        assert!(cache.references(&db, &different_line).unwrap().is_empty());
        let mut different_name = symbols[0].clone();
        different_name.qualified_name = "Different::anchor".into();
        assert!(cache.references(&db, &different_name).unwrap().is_empty());
        assert_eq!(cache.resolution_loads, 2);
        assert_eq!(cache.resolution_hits, 6);
    }

    #[test]
    fn full_shared_resolution_pool_preserves_within_symbol_deduplication() {
        let (db, symbols) = setup_resolution_project();
        for byte_full in [false, true] {
            let mut cache = WalkCache::default();
            if byte_full {
                cache.reference_bytes = WalkCache::MAX_REFERENCE_BYTES;
            } else {
                for index in 0..WalkCache::MAX_SYMBOLS {
                    cache
                        .import_matches
                        .insert(index.to_string(), Arc::new(Vec::new()));
                }
            }
            for sym in &symbols {
                let expected = references_for_symbol(&db, sym).unwrap();
                assert_same_references(&cache.references(&db, sym).unwrap(), &expected);
            }
            assert_eq!(cache.resolution_loads, 4);
            assert_eq!(cache.resolution_hits, 0);
            assert!(cache.resolutions.is_empty());
            assert!(cache.references.is_empty());
            assert!(cache.entry_count() <= WalkCache::MAX_SYMBOLS);
        }
    }

    #[test]
    fn oversized_resolution_key_falls_back_without_retention_or_repeated_decode() {
        let (db, symbols) = setup_resolution_project();
        let mut raw = db.find_references("anchor", None).unwrap();
        raw.truncate(2);
        assert_eq!(raw.len(), 2);
        for row in &mut raw {
            row.from_file = "x".repeat(WalkCache::MAX_REFERENCE_BYTES + 1);
        }
        let expected = resolve_references_to_symbol(&db, &symbols[0], raw.clone(), None).unwrap();
        assert!(expected.is_empty());
        let mut cache = WalkCache::default();
        let actual = resolve_references_to_symbol(&db, &symbols[0], raw, Some(&mut cache)).unwrap();
        assert_same_references(&actual, &expected);
        assert_eq!(cache.resolution_loads, 1);
        assert_eq!(cache.reference_bytes, 0);
        assert!(cache.resolutions.is_empty());
    }

    #[test]
    fn shared_resolution_errors_preserve_full_outgoing_decoder_and_allow_retry() {
        let (db, symbols) = setup_resolution_project();
        db.execute_raw_for_tests(
            "INSERT INTO refs (from_file_id, to_name, to_name_tail, kind, line, col)
             VALUES (3, 'unrelated_noise', 'unrelated_noise', 'invalid_resolution_kind', 4, 0);",
        )
        .unwrap();
        let expected = references_for_symbol(&db, &symbols[0]).unwrap_err();
        let mut cache = WalkCache::default();
        let actual = cache.references(&db, &symbols[0]).unwrap_err();
        assert_eq!(format!("{actual:#}"), format!("{expected:#}"));
        assert!(format!("{actual:#}").contains("invalid_resolution_kind"));
        assert!(
            !cache
                .resolutions
                .contains_key(&("caller.ts".into(), "anchor".into()))
        );
        db.execute_raw_for_tests("DELETE FROM refs WHERE to_name = 'unrelated_noise'")
            .unwrap();
        let retried = cache.references(&db, &symbols[0]).unwrap();
        assert_eq!(retried.len(), 2);
        assert_same_references(&retried, &references_for_symbol(&db, &symbols[0]).unwrap());
    }

    #[test]
    fn fresh_resolution_cache_observes_changed_import_targets() {
        let (db, symbols) = setup_resolution_project();
        let mut before = WalkCache::default();
        assert_eq!(before.references(&db, &symbols[0]).unwrap().len(), 2);
        db.execute_raw_for_tests(
            "UPDATE refs SET to_name = './other', to_name_tail = 'other' WHERE kind = 'import'",
        )
        .unwrap();
        let mut after = WalkCache::default();
        for (sym, count) in symbols.iter().zip([0, 2]) {
            let actual = after.references(&db, sym).unwrap();
            assert_eq!(actual.len(), count);
            assert_same_references(&actual, &references_for_symbol(&db, sym).unwrap());
        }
        assert_eq!(after.resolution_hits, 2);
    }

    #[test]
    fn cancelled_work_cannot_reuse_warm_resolutions_or_reverse_edges() {
        use codesage_protocol::work::{StopReason, WorkControl, WorkStopped};

        let (db, symbols) = setup_resolution_project();
        let mut cache = WalkCache::default();
        assert_eq!(cache.references(&db, &symbols[0]).unwrap().len(), 2);
        let control = WorkControl::new(None);
        control.cancel(StopReason::ClientCancelled);
        let _scope = control.enter();
        for error in [
            cache.resolve(&db, "caller.ts", "anchor").unwrap_err(),
            cache.references(&db, &symbols[0]).unwrap_err(),
        ] {
            assert_eq!(
                error.downcast_ref::<WorkStopped>().unwrap().reason,
                StopReason::ClientCancelled
            );
        }
        assert_eq!(cache.resolution_hits, 0);
        assert_eq!(cache.hits, 0);
    }

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

    fn assert_same_walk(left: &WalkOutcome, right: &WalkOutcome) {
        assert_eq!(
            serde_json::to_value(&left.entries).unwrap(),
            serde_json::to_value(&right.entries).unwrap()
        );
        assert_eq!(left.capped, right.capped);
        assert_eq!(left.seed_count, right.seed_count);
        assert_eq!(left.edge_counts, right.edge_counts);
    }

    #[test]
    fn shared_reverse_edges_preserve_frontiers_and_budget_admission() {
        let (_dir, db) = setup_project_with_uneven_costs();
        let request = ImpactRequest {
            target: ImpactTarget::File {
                path: "Repository.php".into(),
            },
            depth: 2,
            source_only: false,
        };
        let mut cache = WalkCache::default();
        for frontier in [1, MAX_FRONTIER, 8192] {
            let plain = impact_analysis_walk_budgeted(&db, &request, frontier, None).unwrap();
            let shared =
                impact_analysis_walk_shared(&db, &request, frontier, None, Some(&mut cache))
                    .unwrap();
            assert_same_walk(&plain, &shared);
            for steps in [0, 1, 3, 10, usize::MAX] {
                let mut plain_budget = WalkBudget::new(steps, None);
                let mut shared_budget = WalkBudget::new(steps, None);
                let plain =
                    impact_analysis_walk_budgeted(&db, &request, frontier, Some(&mut plain_budget))
                        .unwrap();
                let shared = impact_analysis_walk_shared(
                    &db,
                    &request,
                    frontier,
                    Some(&mut shared_budget),
                    Some(&mut cache),
                )
                .unwrap();
                assert_same_walk(&plain, &shared);
                assert_eq!(plain_budget.remaining, shared_budget.remaining);
                assert_eq!(plain_budget.exhausted, shared_budget.exhausted);
            }
        }
        assert!(
            cache.hits > 0,
            "the walks must actually reuse resolved edges"
        );
        let hits = cache.hits;
        let mut budget = WalkBudget::new(usize::MAX, Some(Instant::now()));
        let expired =
            impact_analysis_walk_shared(&db, &request, 8192, Some(&mut budget), Some(&mut cache))
                .unwrap();
        assert!(expired.capped);
        assert!(expired.entries.is_empty());
        assert_eq!(
            cache.hits, hits,
            "a warm cache cannot bypass an expired deadline"
        );
        let mut full_cache = WalkCache {
            reference_bytes: WalkCache::MAX_REFERENCE_BYTES,
            ..Default::default()
        };
        let plain = impact_analysis_walk_budgeted(&db, &request, 8192, None).unwrap();
        let uncached =
            impact_analysis_walk_shared(&db, &request, 8192, None, Some(&mut full_cache)).unwrap();
        assert_same_walk(&plain, &uncached);
        assert!(full_cache.references.is_empty());
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
    fn file_only_walk_discloses_budget_and_frontier_limits() {
        let dir = tempfile::tempdir().unwrap();
        for (path, source) in [
            ("api.py", "# facade\n"),
            ("a.py", "import api\n"),
            ("b.py", "import api\n"),
            ("outer.py", "import a\nimport b\n"),
        ] {
            std::fs::write(dir.path().join(path), source).unwrap();
        }
        let db = Database::open_in_memory().unwrap();
        crate::full_index(dir.path(), &db, &[], false).unwrap();
        let req = ImpactRequest {
            target: ImpactTarget::File {
                path: "api.py".into(),
            },
            depth: 2,
            source_only: false,
        };
        let complete = impact_analysis_walk_budgeted(&db, &req, MAX_FRONTIER, None).unwrap();
        assert!(!complete.capped);
        assert_eq!(complete.entries.len(), 3);
        assert_eq!(complete.seed_count, 1);
        let limited = impact_analysis_walk_budgeted(&db, &req, 1, None).unwrap();
        assert!(limited.capped);
        assert_eq!(
            limited
                .entries
                .iter()
                .filter(|entry| entry.distance == 1)
                .count(),
            2
        );
        let mut budget = WalkBudget::new(1, None);
        let limited =
            impact_analysis_walk_budgeted(&db, &req, MAX_FRONTIER, Some(&mut budget)).unwrap();
        assert!(limited.capped);
        assert!(limited.entries.is_empty());
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
