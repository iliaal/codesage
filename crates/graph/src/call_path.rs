//! Shortest call chain between two symbols.
//!
//! `find_references` already answers "who calls X" one hop at a time, and
//! `impact_analysis` answers "what does changing X reach" as an unordered set.
//! Neither answers "how does A end up calling B", which is the question behind
//! a security review ("how does request input reach this exec call?") and most
//! unfamiliar-callstack debugging. Walking it by hand means N `find_references`
//! round-trips with the agent holding the frontier in context.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::Result;
use codesage_protocol::{CallPathReport, CallPathRequest, CallPathStep, Symbol};
use codesage_storage::Database;

use crate::bundle::{is_callee_reference, resolve_callee_definitions};

/// Symbols expanded before the search gives up. A hub function reached early
/// can otherwise fan out across most of a large index; the cap keeps a miss
/// bounded in time, and `bounded` in the report tells the caller the answer is
/// "stopped looking", not "no path".
const MAX_VISITED: usize = 4000;

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

    // Seeding every same-named origin at depth 0 searches from all of them at
    // once, which is what a caller asking about a bare name means. The first
    // target hit still yields a shortest path because all seeds start level.
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
            "no call chain within {} hop{} (search stopped at a bound, so a longer path may exist)",
            req.max_depth,
            if req.max_depth == 1 { "" } else { "s" }
        )
    } else {
        format!(
            "'{}' does not reach '{}' through any call chain in the index",
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
    for r in refs {
        if !is_callee_reference(r.kind) {
            continue;
        }
        for def in resolve_callee_definitions(db, &sym.file_path, &r.to_name)? {
            // A symbol whose body spans the call site is its own container,
            // not its callee; without this a recursive or self-referencing
            // definition re-enters itself.
            if def.file_path == sym.file_path
                && def.qualified_name == sym.qualified_name
                && def.line_start == sym.line_start
            {
                continue;
            }
            let k = key_of(&def);
            if seen.insert(k) {
                out.push((def, r.line));
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
    }
}
