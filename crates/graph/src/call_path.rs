//! Shortest call chain between two symbols.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::Result;
use codesage_protocol::{CallPathReport, CallPathRequest, CallPathStep, ReferenceKind, Symbol};
use codesage_storage::Database;

use crate::bundle::resolve_callee_definitions;

/// Control-flow edges only: imports and type relationships do not prove a call.
/// Route handlers count as framework dispatch.
fn is_call_edge(kind: ReferenceKind) -> bool {
    matches!(
        kind,
        ReferenceKind::Call | ReferenceKind::Instantiation | ReferenceKind::RouteHandler
    )
}

/// Bound graph expansion; report exhaustion as incomplete evidence.
const MAX_VISITED: usize = 4000;

/// Bound examined call sites per symbol, including cached duplicate names.
const MAX_REFS_PER_SYMBOL: usize = 200;

/// A definition's identity. `Symbol` carries no id, so key on the triple that
/// locates one — matching `impact.rs`'s `symbol_identity_key`.
type SymbolKey = (String, String, u32);

fn key_of(s: &Symbol) -> SymbolKey {
    (s.file_path.clone(), s.qualified_name.clone(), s.line_start)
}

/// Breadth-first over callee edges, so the first path found is a shortest one.
pub fn trace_call_path(db: &Database, req: &CallPathRequest) -> Result<CallPathReport> {
    let origins = db.find_symbols(&req.from, None)?;
    if origins.is_empty() {
        return Ok(unfound(format!("symbol '{}' not found", req.from), false));
    }
    let targets = db.find_symbols(&req.to, None)?;
    if targets.is_empty() {
        return Ok(unfound(format!("symbol '{}' not found", req.to), false));
    }
    let target_keys: HashSet<SymbolKey> = targets.iter().map(key_of).collect();

    // Equal-depth seeds preserve shortest-path search across same-named origins.
    let mut queue: VecDeque<(Symbol, usize)> = VecDeque::new();
    let mut visited: HashSet<SymbolKey> = HashSet::new();
    // child -> (parent, line in parent's body where the child is called)
    let mut parents: HashMap<SymbolKey, (Symbol, u32)> = HashMap::new();
    let mut origin_keys: HashSet<SymbolKey> = HashSet::new();

    for o in &origins {
        let k = key_of(o);
        origin_keys.insert(k.clone());
        if target_keys.contains(&k) {
            return Ok(CallPathReport {
                found: true,
                steps: vec![step_of(o, None)],
                length: 0,
                note: Some("origin and target are the same symbol".to_string()),
                bounded: false,
                counts_floor: true,
            });
        }
        if visited.insert(k) {
            queue.push_back((o.clone(), 0));
        }
    }

    let mut hit_bound = false;
    while let Some((sym, depth)) = queue.pop_front() {
        if depth >= req.max_depth {
            hit_bound = true;
            continue;
        }
        if visited.len() >= MAX_VISITED {
            hit_bound = true;
            break;
        }
        for callee in callees_of(db, &sym)? {
            let (def, call_line) = callee;
            let k = key_of(&def);
            if target_keys.contains(&k) {
                parents.insert(k.clone(), (sym.clone(), call_line));
                let steps = reconstruct(&def, &parents, &origin_keys);
                let length = steps.len().saturating_sub(1);
                return Ok(CallPathReport {
                    found: true,
                    steps,
                    length,
                    note: None,
                    bounded: false,
                    counts_floor: true,
                });
            }
            if visited.insert(k.clone()) {
                parents.insert(k, (sym.clone(), call_line));
                queue.push_back((def, depth + 1));
            }
        }
    }

    let note = if hit_bound {
        format!(
            "no call chain within {} hop{} via resolved name-based edges (search stopped at a \
             bound, so a longer path may exist)",
            req.max_depth,
            if req.max_depth == 1 { "" } else { "s" }
        )
    } else {
        format!(
            "'{}' does not reach '{}' through any resolved name-based call edge in the index; \
             dynamic dispatch, reflection, and callbacks leave no edge, so this is not proof no \
             path exists",
            req.from, req.to
        )
    };
    Ok(unfound(note, hit_bound))
}

/// Callee definitions invoked inside `sym`'s body, each with the line of the
/// call site. Routes through `resolve_callee_definitions`, so a name with
/// several definitions is narrowed by the calling file's imports rather than
/// fanning out to every same-named symbol.
fn callees_of(db: &Database, sym: &Symbol) -> Result<Vec<(Symbol, u32)>> {
    let refs = db.references_in_file_range(&sym.file_path, sym.line_start, sym.line_end)?;
    let mut out = Vec::new();
    let mut seen: HashSet<SymbolKey> = HashSet::new();
    let mut cache: HashMap<(String, String), Vec<Symbol>> = HashMap::new();
    let mut examined = 0usize;
    for r in refs {
        if !is_call_edge(r.kind) {
            continue;
        }
        // Exclude nested functions' calls; retain rows whose owner is unknown.
        if let Some(owner) = &r.from_symbol
            && owner != &sym.qualified_name
        {
            continue;
        }
        examined += 1;
        if examined > MAX_REFS_PER_SYMBOL {
            tracing::debug!(
                symbol = %sym.qualified_name,
                cap = MAX_REFS_PER_SYMBOL,
                "call-path fan-out capped"
            );
            break;
        }
        let cache_key = (sym.file_path.clone(), r.to_name.clone());
        if !cache.contains_key(&cache_key) {
            let resolved = resolve_callee_definitions(db, &sym.file_path, &r.to_name)?;
            cache.insert(cache_key.clone(), resolved);
        }
        for def in &cache[&cache_key] {
            if def.file_path == sym.file_path
                && def.qualified_name == sym.qualified_name
                && def.line_start == sym.line_start
            {
                continue;
            }
            let k = key_of(def);
            if seen.insert(k) {
                out.push((def.clone(), r.line));
            }
        }
    }
    Ok(out)
}

fn reconstruct(
    target: &Symbol,
    parents: &HashMap<SymbolKey, (Symbol, u32)>,
    origin_keys: &HashSet<SymbolKey>,
) -> Vec<CallPathStep> {
    let mut chain: Vec<(Symbol, Option<u32>)> = Vec::new();
    let mut cur = target.clone();
    let mut call_line = parents.get(&key_of(target)).map(|(_, l)| *l);
    loop {
        let k = key_of(&cur);
        chain.push((cur.clone(), call_line));
        if origin_keys.contains(&k) {
            break;
        }
        match parents.get(&k) {
            Some((parent, _)) => {
                let parent_key = key_of(parent);
                call_line = parents.get(&parent_key).map(|(_, l)| *l);
                cur = parent.clone();
            }
            None => break,
        }
    }
    chain.reverse();
    chain
        .into_iter()
        .map(|(s, line)| step_of(&s, line))
        .collect()
}

fn step_of(s: &Symbol, call_line: Option<u32>) -> CallPathStep {
    CallPathStep {
        name: s.name.clone(),
        qualified_name: s.qualified_name.clone(),
        file_path: s.file_path.clone(),
        line_start: s.line_start,
        call_line,
    }
}

fn unfound(note: String, bounded: bool) -> CallPathReport {
    CallPathReport {
        found: false,
        steps: Vec::new(),
        length: 0,
        note: Some(note),
        bounded,
        counts_floor: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codesage_protocol::{FileInfo, Language, SymbolKind};

    fn define(db: &Database, name: &str, path: &str) {
        let id = db
            .upsert_file(&FileInfo {
                path: path.to_string(),
                language: Language::Rust,
                content_hash: path.to_string(),
            })
            .unwrap();
        db.insert_symbols(
            id,
            &[Symbol {
                name: name.to_string(),
                qualified_name: name.to_string(),
                kind: SymbolKind::Function,
                file_path: path.to_string(),
                line_start: 1,
                line_end: 1,
                col_start: 0,
                col_end: 0,
                rationale: vec![],
            }],
        )
        .unwrap();
    }

    #[test]
    fn not_found_reads_as_floor_over_name_based_edges() {
        let db = Database::open_in_memory().unwrap();
        define(&db, "entry", "entry.rs");
        define(&db, "sink", "sink.rs");
        let req = CallPathRequest {
            from: "entry".to_string(),
            to: "sink".to_string(),
            max_depth: 6,
        };

        let report = trace_call_path(&db, &req).unwrap();
        assert!(!report.found);
        assert!(!report.bounded);
        assert!(report.counts_floor);
        let note = report.note.as_deref().expect("unfound path carries a note");
        assert!(note.contains("name-based"), "{note}");
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["counts_floor"], serde_json::Value::Bool(true));
    }
}
