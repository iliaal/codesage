use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use codesage_protocol::{
    DependencyEntry, FindReferencesRequest, FindReferencesResults, FindSymbolRequest, Reference,
    Symbol, ToResolution,
};
use codesage_storage::Database;

use crate::bundle::{
    import_ref_targets_file, import_ref_targets_symbol, import_refs_for_file,
    resolve_callee_definitions_with_imports,
};
use crate::impact::is_qualified_symbol_name;

pub fn find_symbol(db: &Database, req: &FindSymbolRequest) -> Result<Vec<Symbol>> {
    db.find_symbols(&req.name, req.kind)
}

/// Name-based references, with homonym counts and incomplete-count disclosure.
pub fn find_references(
    db: &Database,
    req: &FindReferencesRequest,
) -> Result<FindReferencesResults> {
    find_references_with_budget(db, req, TO_RESOLUTION_BUDGET)
}

/// [`find_references`] with an explicit wall-clock budget for `to`
/// resolution; the enclosing work deadline still wins when sooner.
pub fn find_references_with_budget(
    db: &Database,
    req: &FindReferencesRequest,
    to_budget: Duration,
) -> Result<FindReferencesResults> {
    let mut results = db.find_references(&req.symbol_name, req.kind)?;
    let definitions = db.find_symbols(&req.symbol_name, None)?;
    let definition_count = definitions.len();
    let to_resolution = attach_handles(db, &mut results, &definitions, to_budget)?;
    let ambiguous = definition_count > 1;
    let note = if ambiguous {
        // Bare candidates cannot disambiguate impact_analysis; offer files instead.
        let qualified: Vec<&str> =
            distinct_sorted(definitions.iter().map(|s| s.qualified_name.as_str()))
                .into_iter()
                .filter(|q| is_qualified_symbol_name(q))
                .collect();
        let bare_only = definitions
            .iter()
            .filter(|s| !is_qualified_symbol_name(&s.qualified_name))
            .count();
        if qualified.len() > 1 {
            let bare_note = if bare_only > 0 {
                format!(
                    " {bare_only} of them carry only the bare name and are reachable by file alone."
                )
            } else {
                String::new()
            };
            Some(format!(
                "{definition_count} definitions share the name '{}'; rows are the union across \
                 all of them. Use find_symbol to list them and impact_analysis with one \
                 qualified name ({}) to scope to one.{bare_note}",
                req.symbol_name,
                sample_list(&qualified, 5)
            ))
        } else {
            let files = distinct_sorted(definitions.iter().map(|s| s.file_path.as_str()));
            let why = if bare_only == definitions.len() {
                "are indistinguishable by qualified name"
            } else {
                "cannot all be addressed by a qualified name, since at most one carries one"
            };
            Some(format!(
                "{definition_count} definitions share the name '{}' and {why}; rows are the \
                 union across all of them. Use find_symbol to list them and scope by file \
                 instead (impact_analysis on the file, or filter rows by from_file): {}.",
                req.symbol_name,
                sample_list(&files, 5)
            ))
        }
    } else if definition_count == 0 {
        Some(format!(
            "no indexed definition named '{}'; references may target an external or unindexed \
             symbol.",
            req.symbol_name
        ))
    } else {
        None
    };
    Ok(FindReferencesResults {
        results,
        counts_floor: true,
        definition_count,
        ambiguous,
        note,
        to_resolution,
    })
}

/// Distinct (caller file, spelling) pairs `find_references` resolves before
/// leaving the remaining rows without `to`.
pub const MAX_TO_RESOLUTION_PAIRS: usize = 256;

/// Wall-clock budget for the whole handle pass; the enclosing work deadline
/// wins when it is sooner.
const TO_RESOLUTION_BUDGET: Duration = Duration::from_millis(750);

/// Fill each row's handles.
///
/// `from` gains `@line` when the enclosing definition is one of several
/// same-named definitions in its file, so it matches the handle
/// `find_symbol` emits for that caller. This pass loads symbols once per
/// caller file and runs for every row, unbounded by the `to` budget.
///
/// `to` is the handle of the one definition the callsite resolves to. It is
/// evidence-gated: bundle resolution may return a lone candidate purely
/// because nothing else shares the name (the rule `impact_analysis` relies
/// on), so the candidate is accepted only when the spelling is qualified and
/// equals its qualified name, when it lives in the caller's file, or when the
/// caller file imports it. Resolution is cached per (caller file, spelling),
/// stops after [`MAX_TO_RESOLUTION_PAIRS`] distinct pairs, and stops at
/// `budget`; either stop is reported through [`ToResolution`] and rows after
/// it carry no `to`.
fn attach_handles(
    db: &Database,
    rows: &mut [Reference],
    definitions: &[Symbol],
    budget: Duration,
) -> Result<Option<ToResolution>> {
    let mut file_symbols: HashMap<String, Arc<Vec<Symbol>>> = HashMap::new();
    for row in rows.iter_mut() {
        codesage_protocol::work::checkpoint()?;
        let Some(caller) = row.from_symbol.as_deref() else {
            continue;
        };
        let symbols = match file_symbols.get(&row.from_file) {
            Some(cached) => Arc::clone(cached),
            None => {
                let loaded = Arc::new(db.symbols_for_file(&row.from_file)?);
                file_symbols.insert(row.from_file.clone(), Arc::clone(&loaded));
                loaded
            }
        };
        row.from_line = enclosing_definition(&symbols, caller, row.line)
            .filter(|s| s.overloaded)
            .map(|s| s.line_start);
    }
    if definitions.is_empty() {
        return Ok(None);
    }

    let mut deadline = Instant::now() + budget;
    if let Some(work) = codesage_protocol::work::current().and_then(|w| w.deadline()) {
        deadline = deadline.min(work);
    }
    let sole_definition = match definitions {
        [only] => Some(only),
        _ => None,
    };
    let mut resolved: HashMap<(String, String), Option<String>> = HashMap::new();
    let mut imports: HashMap<String, Arc<Vec<String>>> = HashMap::new();
    let mut capped = false;
    for row in rows.iter_mut() {
        codesage_protocol::work::checkpoint()?;
        if Instant::now() >= deadline {
            capped = true;
            break;
        }
        // Trivially same-file: the sole definition, spelled as it is defined.
        if let Some(only) = sole_definition
            && only.file_path == row.from_file
            && spelling_names(&row.to_name, only)
        {
            row.to = Some(only.handle().to_string());
            continue;
        }
        let key = (row.from_file.clone(), row.to_name.clone());
        if let Some(cached) = resolved.get(&key) {
            row.to = cached.clone();
            continue;
        }
        if resolved.len() >= MAX_TO_RESOLUTION_PAIRS {
            capped = true;
            continue;
        }
        let from_file = row.from_file.as_str();
        let mut load_imports = || {
            if let Some(cached) = imports.get(from_file) {
                return Ok(Arc::clone(cached));
            }
            let loaded = Arc::new(import_refs_for_file(db, from_file)?);
            imports.insert(from_file.to_string(), Arc::clone(&loaded));
            Ok(loaded)
        };
        let candidates = resolve_callee_definitions_with_imports(
            db,
            from_file,
            &row.to_name,
            &mut load_imports,
        )?;
        let handle = match candidates.as_slice() {
            [only]
                if resolution_has_evidence(from_file, &row.to_name, only, &mut load_imports)? =>
            {
                Some(only.handle().to_string())
            }
            _ => None,
        };
        row.to = handle.clone();
        resolved.insert(key, handle);
    }
    Ok(capped.then_some(ToResolution {
        resolved_pairs: resolved.len(),
        capped,
    }))
}

/// `spelling` names `sym` as defined: its qualified name, or its bare name
/// when the spelling itself is bare. A qualified spelling that differs from
/// the definition's qualified name (`String::newy` vs `newy`) names something
/// else.
fn spelling_names(spelling: &str, sym: &Symbol) -> bool {
    if is_qualified_symbol_name(spelling) {
        sym.qualified_name == spelling
    } else {
        sym.name == spelling || sym.qualified_name == spelling
    }
}

/// Evidence that `candidate` is the definition `spelling` at `caller_file`
/// refers to, beyond being the only one with that name: a qualified spelling
/// that matches, the caller's own file, or an import edge into it.
fn resolution_has_evidence(
    caller_file: &str,
    spelling: &str,
    candidate: &Symbol,
    load_imports: &mut dyn FnMut() -> Result<Arc<Vec<String>>>,
) -> Result<bool> {
    if is_qualified_symbol_name(spelling) && candidate.qualified_name == spelling {
        return Ok(true);
    }
    if candidate.file_path == caller_file {
        return Ok(true);
    }
    let imports = load_imports()?;
    Ok(imports
        .iter()
        .any(|imp| import_ref_targets_symbol(imp, caller_file, spelling, candidate)))
}

/// The innermost definition named `caller` (as `from_symbol` stores it: the
/// qualified name, or the bare name for older rows) whose range holds `line`.
fn enclosing_definition<'a>(symbols: &'a [Symbol], caller: &str, line: u32) -> Option<&'a Symbol> {
    symbols
        .iter()
        .filter(|s| s.qualified_name == caller || s.name == caller)
        .filter(|s| s.line_start <= line && line <= s.line_end)
        .max_by_key(|s| s.line_start)
}

fn distinct_sorted<'a>(items: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut out: Vec<&str> = items.collect();
    out.sort_unstable();
    out.dedup();
    out
}

fn sample_list(items: &[&str], max: usize) -> String {
    let shown = items[..items.len().min(max)].join(", ");
    if items.len() > max {
        format!("{shown}, +{} more", items.len() - max)
    } else {
        shown
    }
}

pub fn list_dependencies(db: &Database, file_path: &str) -> Result<DependencyEntry> {
    let mut out = list_dependencies_batch(db, &[file_path])?;
    Ok(out.pop().expect("batch returns one entry per input path"))
}

/// Resolve dependencies in input order, sharing one project-wide import query.
pub fn list_dependencies_batch(db: &Database, file_paths: &[&str]) -> Result<Vec<DependencyEntry>> {
    let all_refs = db.import_include_refs_all()?;
    let mut out = Vec::with_capacity(file_paths.len());
    for file_path in file_paths {
        let mut entry = db.list_file_dependencies(file_path)?;
        if entry.found {
            resolve_path_imported_by(&mut entry, &all_refs);
        }
        out.push(entry);
    }
    Ok(out)
}

/// Resolve path imports that cannot join against symbol names in SQL.
fn resolve_path_imported_by(entry: &mut DependencyEntry, all_refs: &[(String, String)]) {
    let mut known: HashSet<String> = entry.imported_by.iter().cloned().collect();
    known.insert(entry.file_path.clone());
    for (from_path, to_name) in all_refs {
        if known.contains(from_path) {
            continue;
        }
        if import_ref_targets_file(to_name, from_path, &entry.file_path) {
            known.insert(from_path.clone());
            entry.imported_by.push(from_path.clone());
        }
    }
    entry.imported_by.sort();
}

#[cfg(test)]
mod tests {
    use super::*;
    use codesage_protocol::{FileInfo, Language, Reference, ReferenceKind, SymbolKind};

    fn file(db: &Database, path: &str) -> i64 {
        db.upsert_file(&FileInfo {
            path: path.to_string(),
            language: Language::Rust,
            content_hash: path.to_string(),
        })
        .unwrap()
    }

    fn symbol(name: &str, file_path: &str) -> Symbol {
        qualified_symbol(name, name, file_path)
    }

    fn qualified_symbol(name: &str, qualified_name: &str, file_path: &str) -> Symbol {
        Symbol {
            name: name.to_string(),
            qualified_name: qualified_name.to_string(),
            kind: SymbolKind::Function,
            file_path: file_path.to_string(),
            line_start: 1,
            line_end: 1,
            col_start: 0,
            col_end: 0,
            rationale: vec![],
            overloaded: false,
        }
    }

    fn reference(to_name: &str, file_path: &str) -> Reference {
        Reference {
            from_file: file_path.to_string(),
            from_symbol: None,
            to_name: to_name.to_string(),
            kind: ReferenceKind::Call,
            line: 5,
            col: 12,
            to: None,
            from_line: None,
        }
    }

    fn reference_at(
        to_name: &str,
        file_path: &str,
        from_symbol: Option<&str>,
        line: u32,
        kind: ReferenceKind,
    ) -> Reference {
        Reference {
            from_file: file_path.to_string(),
            from_symbol: from_symbol.map(str::to_string),
            to_name: to_name.to_string(),
            kind,
            line,
            col: 0,
            to: None,
            from_line: None,
        }
    }

    fn symbol_at(name: &str, file_path: &str, line_start: u32, line_end: u32) -> Symbol {
        let mut s = symbol(name, file_path);
        s.line_start = line_start;
        s.line_end = line_end;
        s
    }

    fn to_of(out: &FindReferencesResults, from_file: &str, to_name: &str) -> Option<String> {
        out.results
            .iter()
            .find(|r| r.from_file == from_file && r.to_name == to_name)
            .unwrap_or_else(|| panic!("row {from_file} -> {to_name} in {:?}", out.results))
            .to
            .clone()
    }

    fn lookup(db: &Database, name: &str) -> FindReferencesResults {
        find_references(
            db,
            &FindReferencesRequest {
                symbol_name: name.to_string(),
                kind: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn envelope_with_no_definition_flags_external_target() {
        let db = Database::open_in_memory().unwrap();
        let caller = file(&db, "caller.rs");
        db.insert_references(caller, &[reference("helper", "caller.rs")])
            .unwrap();

        let out = lookup(&db, "helper");
        assert_eq!(out.results.len(), 1);
        assert!(out.counts_floor);
        assert_eq!(out.definition_count, 0);
        assert!(!out.ambiguous);
        let note = out.note.expect("zero definitions must carry a note");
        assert!(note.contains("no indexed definition"), "{note}");
    }

    #[test]
    fn envelope_with_one_definition_is_unambiguous_and_silent() {
        let db = Database::open_in_memory().unwrap();
        let a = file(&db, "a.rs");
        db.insert_symbols(a, &[symbol("helper", "a.rs")]).unwrap();

        let out = lookup(&db, "helper");
        assert!(out.results.is_empty());
        assert!(out.counts_floor, "a zero must still read as a floor");
        assert_eq!(out.definition_count, 1);
        assert!(!out.ambiguous);
        assert!(out.note.is_none(), "note: {:?}", out.note);
        let json = serde_json::to_value(&out).unwrap();
        assert_eq!(json["counts_floor"], serde_json::Value::Bool(true));
        assert!(json.get("note").is_none(), "{json}");
    }

    #[test]
    fn envelope_with_same_qualified_name_in_two_files_scopes_by_file() {
        let db = Database::open_in_memory().unwrap();
        let a = file(&db, "a.rs");
        let b = file(&db, "b.rs");
        db.insert_symbols(a, &[symbol("helper", "a.rs")]).unwrap();
        db.insert_symbols(b, &[symbol("helper", "b.rs")]).unwrap();
        db.insert_references(a, &[reference("helper", "a.rs")])
            .unwrap();
        db.insert_references(b, &[reference("helper", "b.rs")])
            .unwrap();

        let out = lookup(&db, "helper");
        assert_eq!(
            out.results.len(),
            2,
            "rows are the union: {:?}",
            out.results
        );
        assert_eq!(out.definition_count, 2);
        assert!(out.ambiguous);
        let note = out.note.expect("ambiguous lookup must carry a note");
        assert!(
            note.starts_with("2 definitions share the name 'helper'"),
            "{note}"
        );
        assert!(note.contains("find_symbol"), "{note}");
        assert!(note.contains("impact_analysis"), "{note}");
        assert!(note.contains("indistinguishable"), "{note}");
        assert!(note.contains("a.rs, b.rs"), "{note}");
        assert!(!note.contains("qualified name ("), "{note}");
    }

    #[test]
    fn envelope_with_two_distinct_qualified_names_lists_them() {
        let db = Database::open_in_memory().unwrap();
        let a = file(&db, "a.rs");
        let b = file(&db, "b.rs");
        db.insert_symbols(a, &[qualified_symbol("helper", "alpha::helper", "a.rs")])
            .unwrap();
        db.insert_symbols(b, &[qualified_symbol("helper", "beta::helper", "b.rs")])
            .unwrap();

        let out = lookup(&db, "helper");
        assert_eq!(out.definition_count, 2);
        assert!(out.ambiguous);
        let note = out.note.expect("ambiguous lookup must carry a note");
        assert!(note.contains("impact_analysis"), "{note}");
        assert!(note.contains("alpha::helper, beta::helper"), "{note}");
        assert!(!note.contains("indistinguishable"), "{note}");
    }

    #[test]
    fn envelope_with_one_bare_and_one_qualified_name_scopes_by_file() {
        let db = Database::open_in_memory().unwrap();
        let a = file(&db, "a.rs");
        let b = file(&db, "b.rs");
        db.insert_symbols(a, &[symbol("helper", "a.rs")]).unwrap();
        db.insert_symbols(b, &[qualified_symbol("helper", "beta::helper", "b.rs")])
            .unwrap();

        let out = lookup(&db, "helper");
        assert_eq!(out.definition_count, 2);
        assert!(out.ambiguous);
        let note = out.note.expect("ambiguous lookup must carry a note");
        assert!(note.contains("at most one carries one"), "{note}");
        assert!(!note.contains("indistinguishable"), "{note}");
        assert!(note.contains("a.rs, b.rs"), "{note}");
        assert!(!note.contains("qualified name ("), "{note}");
        assert!(!note.contains("beta::helper"), "{note}");
    }

    #[test]
    fn envelope_with_two_qualified_and_one_bare_counts_the_bare_one() {
        let db = Database::open_in_memory().unwrap();
        let a = file(&db, "a.rs");
        let b = file(&db, "b.rs");
        let c = file(&db, "c.rs");
        db.insert_symbols(a, &[qualified_symbol("build", "Foo::build", "a.rs")])
            .unwrap();
        db.insert_symbols(b, &[qualified_symbol("build", "Bar::build", "b.rs")])
            .unwrap();
        db.insert_symbols(c, &[symbol("build", "c.rs")]).unwrap();

        let out = lookup(&db, "build");
        assert_eq!(out.definition_count, 3);
        let note = out.note.expect("ambiguous lookup must carry a note");
        assert!(note.contains("Bar::build, Foo::build"), "{note}");
        assert!(
            note.contains("1 of them carry only the bare name"),
            "{note}"
        );
    }

    /// Two same-named definitions in different files force real resolution
    /// for every caller file; past the pair cap, rows keep no `to` and the
    /// envelope says so.
    #[test]
    fn to_resolution_caps_distinct_pairs_and_discloses_it() {
        let db = Database::open_in_memory().unwrap();
        let a = file(&db, "def_a.rs");
        let b = file(&db, "def_b.rs");
        db.insert_symbols(a, &[symbol("target", "def_a.rs")])
            .unwrap();
        db.insert_symbols(b, &[symbol("target", "def_b.rs")])
            .unwrap();
        let callers = MAX_TO_RESOLUTION_PAIRS + 3;
        for i in 0..callers {
            let path = format!("caller_{i:03}.rs");
            let id = file(&db, &path);
            db.insert_references(id, &[reference("target", &path)])
                .unwrap();
        }

        let out = lookup(&db, "target");
        assert_eq!(out.results.len(), callers);
        assert_eq!(
            out.to_resolution,
            Some(ToResolution {
                resolved_pairs: MAX_TO_RESOLUTION_PAIRS,
                capped: true,
            })
        );
        let json = serde_json::to_value(&out).unwrap();
        assert_eq!(json["to_resolution"]["capped"], true, "{json}");
    }

    #[test]
    fn to_resolution_is_silent_under_the_cap() {
        let db = Database::open_in_memory().unwrap();
        let a = file(&db, "def_a.rs");
        let b = file(&db, "def_b.rs");
        db.insert_symbols(a, &[symbol("target", "def_a.rs")])
            .unwrap();
        db.insert_symbols(b, &[symbol("target", "def_b.rs")])
            .unwrap();
        for i in 0..3 {
            let path = format!("caller_{i}.rs");
            let id = file(&db, &path);
            db.insert_references(id, &[reference("target", &path)])
                .unwrap();
        }

        let out = lookup(&db, "target");
        assert_eq!(out.results.len(), 3);
        assert_eq!(out.to_resolution, None);
        let json = serde_json::to_value(&out).unwrap();
        assert!(json.get("to_resolution").is_none(), "{json}");
    }

    /// One definition in the caller's own file resolves without a
    /// resolution pass at all.
    #[test]
    fn same_file_sole_definition_resolves_to_without_pairs() {
        let db = Database::open_in_memory().unwrap();
        let a = file(&db, "a.rs");
        db.insert_symbols(a, &[symbol("helper", "a.rs")]).unwrap();
        db.insert_references(a, &[reference("helper", "a.rs")])
            .unwrap();

        let out = lookup(&db, "helper");
        assert_eq!(out.results[0].to.as_deref(), Some("sym:a.rs#helper"));
        assert_eq!(out.to_resolution, None);
    }

    /// The same-file shortcut applies only when the spelling names the sole
    /// definition as it is defined: `String::newy` is not `newy`.
    #[test]
    fn same_file_shortcut_requires_the_spelling_to_name_the_definition() {
        let db = Database::open_in_memory().unwrap();
        let e = file(&db, "src/e.rs");
        db.insert_symbols(e, &[symbol("newy", "src/e.rs")]).unwrap();
        db.insert_references(
            e,
            &[
                reference_at("String::newy", "src/e.rs", None, 5, ReferenceKind::Call),
                reference_at("newy", "src/e.rs", None, 6, ReferenceKind::Call),
            ],
        )
        .unwrap();

        let out = lookup(&db, "newy");
        assert_eq!(out.definition_count, 1);
        assert_eq!(to_of(&out, "src/e.rs", "String::newy"), None);
        assert_eq!(
            to_of(&out, "src/e.rs", "newy").as_deref(),
            Some("sym:src/e.rs#newy")
        );
    }

    /// A lone candidate is not evidence: `spawn` from a file that does not
    /// import it, or spelled `tokio::spawn`, stays unresolved; the caller's
    /// own file and an import edge resolve it.
    #[test]
    fn to_requires_qualified_same_file_or_import_evidence() {
        let db = Database::open_in_memory().unwrap();
        let util = file(&db, "src/util.rs");
        let a = file(&db, "src/a.rs");
        let b = file(&db, "src/b.rs");
        let c = file(&db, "src/c.rs");
        db.insert_symbols(util, &[symbol("spawn", "src/util.rs")])
            .unwrap();
        db.insert_references(
            util,
            &[reference_at(
                "spawn",
                "src/util.rs",
                None,
                9,
                ReferenceKind::Call,
            )],
        )
        .unwrap();
        db.insert_references(
            a,
            &[reference_at(
                "spawn",
                "src/a.rs",
                None,
                3,
                ReferenceKind::Call,
            )],
        )
        .unwrap();
        db.insert_references(
            b,
            &[reference_at(
                "tokio::spawn",
                "src/b.rs",
                None,
                3,
                ReferenceKind::Call,
            )],
        )
        .unwrap();
        db.insert_references(
            c,
            &[
                reference_at(
                    "crate::util::spawn",
                    "src/c.rs",
                    None,
                    1,
                    ReferenceKind::Import,
                ),
                reference_at("spawn", "src/c.rs", None, 4, ReferenceKind::Call),
            ],
        )
        .unwrap();

        let out = lookup(&db, "spawn");
        assert_eq!(out.definition_count, 1);
        assert_eq!(
            to_of(&out, "src/a.rs", "spawn"),
            None,
            "no import, no evidence"
        );
        assert_eq!(
            to_of(&out, "src/b.rs", "tokio::spawn"),
            None,
            "other namespace"
        );
        assert_eq!(
            to_of(&out, "src/util.rs", "spawn").as_deref(),
            Some("sym:src/util.rs#spawn"),
            "same file"
        );
        assert_eq!(
            to_of(&out, "src/c.rs", "spawn").as_deref(),
            Some("sym:src/util.rs#spawn"),
            "imported"
        );
        assert_eq!(out.to_resolution, None);
    }

    /// `from` handles are computed for every row before the `to` budget is
    /// consulted, so an exhausted budget caps `to` without dropping `@line`.
    #[test]
    fn from_line_survives_an_exhausted_to_budget() {
        let db = Database::open_in_memory().unwrap();
        let a = file(&db, "a.cpp");
        let b = file(&db, "b.cpp");
        let c = file(&db, "c.cpp");
        db.insert_symbols(
            a,
            &[
                symbol_at("run", "a.cpp", 1, 5),
                symbol_at("run", "a.cpp", 10, 15),
            ],
        )
        .unwrap();
        db.insert_symbols(b, &[symbol("helper", "b.cpp")]).unwrap();
        db.insert_symbols(c, &[symbol("helper", "c.cpp")]).unwrap();
        db.insert_references(
            a,
            &[
                reference_at("helper", "a.cpp", Some("run"), 3, ReferenceKind::Call),
                reference_at("helper", "a.cpp", Some("run"), 12, ReferenceKind::Call),
            ],
        )
        .unwrap();

        let out = find_references_with_budget(
            &db,
            &FindReferencesRequest {
                symbol_name: "helper".to_string(),
                kind: None,
            },
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(
            out.to_resolution,
            Some(ToResolution {
                resolved_pairs: 0,
                capped: true,
            })
        );
        let mut froms: Vec<String> = out
            .results
            .iter()
            .map(|r| r.from_handle().unwrap().to_string())
            .collect();
        froms.sort();
        assert_eq!(froms, ["sym:a.cpp#run@1", "sym:a.cpp#run@10"]);
        assert!(out.results.iter().all(|r| r.to.is_none()));
    }

    /// A zero-definition query resolves nothing and must not claim a cap.
    #[test]
    fn no_definitions_means_no_to_resolution_envelope() {
        let db = Database::open_in_memory().unwrap();
        let caller = file(&db, "caller.rs");
        db.insert_references(caller, &[reference("helper", "caller.rs")])
            .unwrap();
        let out = find_references_with_budget(
            &db,
            &FindReferencesRequest {
                symbol_name: "helper".to_string(),
                kind: None,
            },
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(out.to_resolution, None);
    }

    #[test]
    fn list_dependencies_omits_handle_for_paths_outside_the_repository() {
        let db = Database::open_in_memory().unwrap();
        for hostile in ["../../../etc/passwd", "/etc/passwd"] {
            let entry = list_dependencies(&db, hostile).unwrap();
            assert!(!entry.found);
            assert_eq!(entry.handle, "");
            let json = serde_json::to_value(&entry).unwrap();
            assert!(json.get("handle").is_none(), "{json}");
        }
    }

    #[test]
    fn sample_list_counts_the_overflow() {
        let items = ["a", "b", "c", "d", "e", "f", "g"];
        assert_eq!(sample_list(&items, 5), "a, b, c, d, e, +2 more");
        assert_eq!(sample_list(&items[..2], 5), "a, b");
    }
}
