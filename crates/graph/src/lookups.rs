use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use codesage_protocol::{
    DependencyEntry, FindReferencesRequest, FindReferencesResults, FindSymbolRequest, Reference,
    ReferenceKind, Symbol, SymbolKind, ToResolution,
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
    let mut caches = EvidenceCaches::new(db);
    for row in rows.iter_mut() {
        codesage_protocol::work::checkpoint()?;
        let Some(caller) = row.from_symbol.as_deref() else {
            continue;
        };
        let symbols = caches.symbols(&row.from_file)?;
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
        let candidates =
            resolve_callee_definitions_with_imports(db, from_file, &row.to_name, &mut || {
                caches.imports(from_file)
            })?;
        let handle = match candidates.as_slice() {
            [only] if caches.has_evidence(from_file, &row.to_name, only)? => {
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

/// Per-caller-file lookups shared by the `from` and `to` passes, each loaded
/// at most once per file.
struct EvidenceCaches<'a> {
    db: &'a Database,
    symbols: HashMap<String, Arc<Vec<Symbol>>>,
    imports: HashMap<String, Arc<Vec<String>>>,
    outgoing: HashMap<String, Arc<Vec<(String, ReferenceKind)>>>,
}

impl<'a> EvidenceCaches<'a> {
    fn new(db: &'a Database) -> Self {
        Self {
            db,
            symbols: HashMap::new(),
            imports: HashMap::new(),
            outgoing: HashMap::new(),
        }
    }

    fn symbols(&mut self, file: &str) -> Result<Arc<Vec<Symbol>>> {
        if let Some(cached) = self.symbols.get(file) {
            return Ok(Arc::clone(cached));
        }
        let loaded = Arc::new(self.db.symbols_for_file(file)?);
        self.symbols.insert(file.to_string(), Arc::clone(&loaded));
        Ok(loaded)
    }

    /// The import-shaped refs bundle resolution consumes.
    fn imports(&mut self, file: &str) -> Result<Arc<Vec<String>>> {
        if let Some(cached) = self.imports.get(file) {
            return Ok(Arc::clone(cached));
        }
        let loaded = Arc::new(import_refs_for_file(self.db, file)?);
        self.imports.insert(file.to_string(), Arc::clone(&loaded));
        Ok(loaded)
    }

    /// Every outgoing ref of `file`, with its kind.
    fn outgoing(&mut self, file: &str) -> Result<Arc<Vec<(String, ReferenceKind)>>> {
        if let Some(cached) = self.outgoing.get(file) {
            return Ok(Arc::clone(cached));
        }
        let rows = match self.db.file_id_for_path(file)? {
            Some(id) => self.db.refs_outgoing_for_file_id(id)?,
            None => Vec::new(),
        };
        let loaded = Arc::new(rows);
        self.outgoing.insert(file.to_string(), Arc::clone(&loaded));
        Ok(loaded)
    }

    /// Evidence that `candidate` is the definition `spelling` at `caller_file`
    /// refers to, beyond being the only one with that name: a qualified
    /// spelling that matches, the caller's own file, an import edge into the
    /// symbol, an import or type reference naming its owner, or (C/C++) an
    /// include of the header that pairs with its file (`util.h` for
    /// `util.c`). A header that itself declares the name is a second
    /// candidate, which bundle resolution already narrows by include path.
    fn has_evidence(
        &mut self,
        caller_file: &str,
        spelling: &str,
        candidate: &Symbol,
    ) -> Result<bool> {
        if is_qualified_symbol_name(spelling) && candidate.qualified_name == spelling {
            return Ok(true);
        }
        if candidate.file_path == caller_file {
            return Ok(true);
        }
        let imports = self.imports(caller_file)?;
        if imports
            .iter()
            .any(|imp| import_ref_targets_symbol(imp, caller_file, spelling, candidate))
        {
            return Ok(true);
        }
        if let Some(owner) = owner_of(&candidate.qualified_name) {
            if imports.iter().any(|imp| names_owner(imp, owner)) {
                return Ok(true);
            }
            if candidate.kind == SymbolKind::Method {
                let owner_tail = last_segment(owner);
                let outgoing = self.outgoing(caller_file)?;
                if outgoing.iter().any(|(name, kind)| {
                    matches!(
                        kind,
                        ReferenceKind::Import
                            | ReferenceKind::ImportBinding
                            | ReferenceKind::TypeHint
                            | ReferenceKind::Instantiation
                    ) && last_segment(name) == owner_tail
                }) {
                    return Ok(true);
                }
            }
        }
        if is_c_family(caller_file) && is_c_family(&candidate.file_path) {
            let candidate_stem = file_stem(&candidate.file_path);
            let outgoing = self.outgoing(caller_file)?;
            if outgoing.iter().any(|(name, kind)| {
                *kind == ReferenceKind::Include
                    && file_stem(name.trim_matches(|c| matches!(c, '<' | '>' | '"')))
                        == candidate_stem
            }) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

const SEGMENT_SEPARATORS: [&str; 3] = ["::", "\\", "."];

/// `qualified` without its last segment: `App\Lib\Foo::bar` → `App\Lib\Foo`,
/// `Database::open` → `Database`; `None` for a bare name.
fn owner_of(qualified: &str) -> Option<&str> {
    SEGMENT_SEPARATORS
        .iter()
        .filter_map(|sep| qualified.rfind(sep))
        .max()
        .map(|pos| &qualified[..pos])
        .filter(|owner| !owner.is_empty())
}

/// The last segment of a qualified or path-like name.
fn last_segment(name: &str) -> &str {
    let cut = SEGMENT_SEPARATORS
        .iter()
        .map(|sep| (sep, name.rfind(sep)))
        .filter_map(|(sep, pos)| pos.map(|p| p + sep.len()))
        .chain(name.rfind('/').map(|p| p + 1))
        .max()
        .unwrap_or(0);
    &name[cut..]
}

/// An import target names `owner` when it is the owner or ends in the owner
/// at a segment boundary (`use codesage_storage::Database` for
/// `Database::open`). A namespace import (`use App\Lib;`) is an ancestor,
/// not the owner, and names nothing here.
fn names_owner(import: &str, owner: &str) -> bool {
    if import == owner {
        return true;
    }
    SEGMENT_SEPARATORS.iter().any(|sep| {
        import
            .strip_suffix(owner)
            .is_some_and(|head| head.ends_with(sep))
    })
}

fn is_c_family(path: &str) -> bool {
    matches!(
        path.rsplit_once('.').map(|(_, ext)| ext),
        Some("c" | "h" | "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx")
    )
}

fn file_stem(path: &str) -> &str {
    let name = path.rsplit_once('/').map_or(path, |(_, name)| name);
    name.rsplit_once('.').map_or(name, |(stem, _)| stem)
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

    fn method_symbol(name: &str, qualified_name: &str, file_path: &str) -> Symbol {
        let mut s = qualified_symbol(name, qualified_name, file_path);
        s.kind = SymbolKind::Method;
        s
    }

    /// PHP: `use App\Lib\Foo;` names the owner of `App\Lib\Foo\phpbar`, so
    /// a call spelled by the bare method name resolves from that file only.
    #[test]
    fn owner_import_resolves_a_method_of_the_imported_class() {
        let db = Database::open_in_memory().unwrap();
        let foo = file(&db, "src/Lib/Foo.php");
        let ctl = file(&db, "src/Ctl.php");
        let stranger = file(&db, "src/Other.php");
        db.insert_symbols(
            foo,
            &[method_symbol(
                "phpbar",
                "App\\Lib\\Foo\\phpbar",
                "src/Lib/Foo.php",
            )],
        )
        .unwrap();
        db.insert_references(
            ctl,
            &[
                reference_at(
                    "App\\Lib\\Foo",
                    "src/Ctl.php",
                    None,
                    3,
                    ReferenceKind::Import,
                ),
                reference_at("phpbar", "src/Ctl.php", None, 9, ReferenceKind::Call),
            ],
        )
        .unwrap();
        db.insert_references(
            stranger,
            &[reference_at(
                "phpbar",
                "src/Other.php",
                None,
                9,
                ReferenceKind::Call,
            )],
        )
        .unwrap();
        let namespace_only = file(&db, "src/Ns.php");
        db.insert_references(
            namespace_only,
            &[
                reference_at("App\\Lib", "src/Ns.php", None, 3, ReferenceKind::Import),
                reference_at("phpbar", "src/Ns.php", None, 9, ReferenceKind::Call),
            ],
        )
        .unwrap();

        let out = lookup(&db, "phpbar");
        assert_eq!(
            to_of(&out, "src/Ns.php", "phpbar"),
            None,
            "`use App\\Lib;` is an ancestor namespace, not the owner"
        );
        assert_eq!(
            to_of(&out, "src/Ctl.php", "phpbar").as_deref(),
            Some("sym:src/Lib/Foo.php#App\\Lib\\Foo\\phpbar")
        );
        assert_eq!(
            to_of(&out, "src/Other.php", "phpbar"),
            None,
            "no import names the owner"
        );
    }

    /// C: `#include "util.h"` pairs with `util.c`; an unrelated include is
    /// not evidence.
    #[test]
    fn include_stem_resolves_c_definitions() {
        let db = Database::open_in_memory().unwrap();
        let util = file(&db, "src/util.c");
        let a = file(&db, "src/a.c");
        let c = file(&db, "src/c.c");
        db.insert_symbols(util, &[symbol("do_work", "src/util.c")])
            .unwrap();
        db.insert_references(
            a,
            &[
                reference_at("util.h", "src/a.c", None, 1, ReferenceKind::Include),
                reference_at("do_work", "src/a.c", None, 7, ReferenceKind::Call),
            ],
        )
        .unwrap();
        db.insert_references(
            c,
            &[
                reference_at("<stdio.h>", "src/c.c", None, 1, ReferenceKind::Include),
                reference_at("do_work", "src/c.c", None, 7, ReferenceKind::Call),
            ],
        )
        .unwrap();

        let out = lookup(&db, "do_work");
        assert_eq!(
            to_of(&out, "src/a.c", "do_work").as_deref(),
            Some("sym:src/util.c#do_work")
        );
        assert_eq!(to_of(&out, "src/c.c", "do_work"), None, "unrelated include");
    }

    /// Rust: `use codesage_storage::Database;` or a `Database` type hint
    /// vouches for `db.upsert_file()` resolving to `Database::upsert_file`;
    /// `String::new` and a bare `new` without owner evidence stay unresolved.
    #[test]
    fn owner_tail_and_type_reference_resolve_cross_file_methods() {
        let db = Database::open_in_memory().unwrap();
        let storage = file(&db, "crates/storage/src/db/mod.rs");
        let importer = file(&db, "crates/graph/src/x.rs");
        let hinted = file(&db, "crates/graph/src/y.rs");
        let stranger = file(&db, "crates/graph/src/z.rs");
        db.insert_symbols(
            storage,
            &[
                method_symbol(
                    "upsert_file",
                    "Database::upsert_file",
                    "crates/storage/src/db/mod.rs",
                ),
                method_symbol("new", "OverviewCache::new", "crates/storage/src/db/mod.rs"),
            ],
        )
        .unwrap();
        db.insert_references(
            importer,
            &[
                reference_at(
                    "codesage_storage::Database",
                    "crates/graph/src/x.rs",
                    None,
                    1,
                    ReferenceKind::Import,
                ),
                reference_at(
                    "upsert_file",
                    "crates/graph/src/x.rs",
                    None,
                    8,
                    ReferenceKind::Call,
                ),
                reference_at(
                    "String::new",
                    "crates/graph/src/x.rs",
                    None,
                    9,
                    ReferenceKind::Call,
                ),
                reference_at(
                    "new",
                    "crates/graph/src/x.rs",
                    None,
                    10,
                    ReferenceKind::Call,
                ),
            ],
        )
        .unwrap();
        db.insert_references(
            hinted,
            &[
                reference_at(
                    "Database",
                    "crates/graph/src/y.rs",
                    None,
                    2,
                    ReferenceKind::TypeHint,
                ),
                reference_at(
                    "upsert_file",
                    "crates/graph/src/y.rs",
                    None,
                    8,
                    ReferenceKind::Call,
                ),
            ],
        )
        .unwrap();
        db.insert_references(
            stranger,
            &[reference_at(
                "upsert_file",
                "crates/graph/src/z.rs",
                None,
                8,
                ReferenceKind::Call,
            )],
        )
        .unwrap();

        let out = lookup(&db, "upsert_file");
        let expected = Some("sym:crates/storage/src/db/mod.rs#Database::upsert_file");
        assert_eq!(
            to_of(&out, "crates/graph/src/x.rs", "upsert_file").as_deref(),
            expected
        );
        assert_eq!(
            to_of(&out, "crates/graph/src/y.rs", "upsert_file").as_deref(),
            expected
        );
        assert_eq!(to_of(&out, "crates/graph/src/z.rs", "upsert_file"), None);

        let out = lookup(&db, "new");
        assert_eq!(to_of(&out, "crates/graph/src/x.rs", "String::new"), None);
        assert_eq!(to_of(&out, "crates/graph/src/x.rs", "new"), None);
    }

    #[test]
    fn owner_helpers_split_on_language_separators() {
        assert_eq!(owner_of("App\\Lib\\Foo::bar"), Some("App\\Lib\\Foo"));
        assert_eq!(owner_of("Database::open"), Some("Database"));
        assert_eq!(owner_of("pkg.Class.method"), Some("pkg.Class"));
        assert_eq!(owner_of("bare"), None);
        assert_eq!(last_segment("codesage_storage::Database"), "Database");
        assert_eq!(last_segment("App\\Lib\\Foo"), "Foo");
        assert_eq!(last_segment("pkg.Class"), "Class");
        assert!(names_owner("codesage_storage::Database", "Database"));
        assert!(
            !names_owner("App\\Lib", "App\\Lib\\Foo"),
            "a namespace import is an ancestor, not the owner"
        );
        assert!(!names_owner("App\\Library", "App\\Lib\\Foo"));
        assert!(names_owner("App\\Lib\\Foo", "App\\Lib\\Foo"));
        assert!(!names_owner("tokio", "OverviewCache"));
        assert_eq!(file_stem("src/util.c"), "util");
        assert_eq!(file_stem("util.h"), "util");
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
