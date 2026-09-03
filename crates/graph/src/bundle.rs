use std::collections::HashSet;
use std::sync::OnceLock;

use anyhow::Result;
use codesage_protocol::{
    ContextBundle, ExportRequest, ReferenceKind, SearchRequest, SearchResult, Symbol, SymbolSummary,
};
use codesage_storage::Database;

use crate::impact::{is_qualified_symbol_name, references_for_symbol};
use crate::search::{RerankFn, annotate_with_symbols, env_default_on, parse_db_language, search};

/// Default-on; opt-out via `CODESAGE_BUNDLE_LINE_NUMBERS=0` (or "false").
static BUNDLE_LINE_NUMBERS_ENABLED: OnceLock<bool> = OnceLock::new();

fn bundle_line_numbers_enabled() -> bool {
    *BUNDLE_LINE_NUMBERS_ENABLED.get_or_init(|| env_default_on("CODESAGE_BUNDLE_LINE_NUMBERS"))
}

/// `SymbolKind` render strings, used to recognize a chunk-augmentation
/// header's `# <name> (<kind>)` lines without mistaking a source comment
/// like `# cleanup (later)` for one.
const SYMBOL_KIND_STRS: [&str; 11] = [
    "function",
    "method",
    "class",
    "trait",
    "interface",
    "struct",
    "enum",
    "constant",
    "macro",
    "module",
    "namespace",
];

fn is_symbol_header_line(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("# ") else {
        return false;
    };
    let Some(open) = rest.rfind(" (") else {
        return false;
    };
    let Some(kind) = rest[open + 2..].strip_suffix(')') else {
        return false;
    };
    SYMBOL_KIND_STRS.contains(&kind)
}

/// Prefix the body lines of a bundle chunk with 1-based file line numbers
/// (`  12 | code`) starting at `start_line`, so an agent can cite
/// `file:line` straight from the bundle without re-reading. Applied at read
/// time only — stored and embedded chunk text is never touched.
///
/// The augmentation header (`# <file_path>` plus `# <symbol> (<kind>)`
/// lines that `semantic.rs` prepends) passes through unnumbered. Detection
/// is anchored on the chunk's own `file_path` for line 1, so a C/Rust chunk
/// (no header) or a source line that merely starts with `#` is never
/// mistaken for header.
fn number_chunk_lines(content: &str, file_path: &str, start_line: u32) -> String {
    if content.is_empty() {
        return String::new();
    }
    let lines: Vec<&str> = content.lines().collect();
    let mut idx = 0;
    let header_anchor = format!("# {file_path}");
    if lines.first() == Some(&header_anchor.as_str()) {
        idx = 1;
        while idx < lines.len() && is_symbol_header_line(lines[idx]) {
            idx += 1;
        }
    }
    let body_count = lines.len() - idx;
    let last_no = start_line as usize + body_count.saturating_sub(1);
    let width = last_no.to_string().len().max(4);

    let mut out = String::with_capacity(content.len() + body_count * (width + 4));
    for header_line in &lines[..idx] {
        out.push_str(header_line);
        out.push('\n');
    }
    for (offset, body_line) in lines[idx..].iter().enumerate() {
        let n = start_line as usize + offset;
        out.push_str(&format!("{n:>width$} | {body_line}\n"));
    }
    // `lines()` drops a trailing newline; we always append one per line.
    // Restore the original trailing-newline shape so the chunk doesn't grow.
    if !content.ends_with('\n') {
        out.pop();
    }
    out
}

/// Apply read-time line numbering to every chunk in a bundle, when enabled.
fn finalize_bundle(mut bundle: ContextBundle) -> ContextBundle {
    if bundle_line_numbers_enabled() {
        for r in bundle.primary.iter_mut().chain(bundle.related.iter_mut()) {
            r.content = number_chunk_lines(&r.content, &r.file_path, r.start_line);
        }
    }
    bundle
}

pub fn export_context(
    db: &Database,
    query_embedding: &[f32],
    rerank: Option<RerankFn<'_>>,
    req: &ExportRequest,
) -> Result<ContextBundle> {
    if let Some(sym_name) = &req.symbol {
        return export_context_for_symbol(db, sym_name, req);
    }

    let query = req.query.as_deref().unwrap_or_default();
    if query.is_empty() {
        anyhow::bail!("export_context requires either `query` or `symbol`");
    }

    let search_req = SearchRequest {
        query: query.to_string(),
        limit: Some(req.limit),
        offset: Some(0),
        languages: None,
        paths: None,
    };
    let primary = search(db, query_embedding, rerank, &search_req)?;

    let mut symbol_defs: Vec<Symbol> = Vec::new();
    let mut seen_sym: HashSet<String> = HashSet::new();
    let mut related: Vec<SearchResult> = Vec::new();
    let mut related_keys: HashSet<(String, u32)> = primary
        .iter()
        .map(|r| (r.file_path.clone(), r.start_line))
        .collect();

    for result in &primary {
        for sum in &result.symbols {
            if !seen_sym.insert(sum.qualified_name.clone()) {
                continue;
            }
            if let Some(d) = find_definition_for_summary(db, sum, &result.file_path)? {
                symbol_defs.push(d);
            }
        }
    }

    if req.include_callees || req.include_callers {
        let related_symbols: Vec<Symbol> = symbol_defs.iter().take(5).cloned().collect();
        add_related_for_symbols(
            db,
            &related_symbols,
            req.include_callers,
            req.include_callees,
            req.limit,
            &mut related,
            &mut related_keys,
        )?;
    }

    Ok(finalize_bundle(ContextBundle {
        found: true,
        target_description: format!("query: {query}"),
        primary,
        related,
        symbol_definitions: symbol_defs,
    }))
}

fn find_definition_for_summary(
    db: &Database,
    summary: &SymbolSummary,
    file_path: &str,
) -> Result<Option<Symbol>> {
    let candidates: Vec<Symbol> = db
        .find_symbols(&summary.name, Some(summary.kind))?
        .into_iter()
        .filter(|d| d.qualified_name == summary.qualified_name)
        .collect();

    if let Some(same_file) = candidates.iter().find(|d| d.file_path == file_path) {
        return Ok(Some(same_file.clone()));
    }

    Ok(candidates.into_iter().next())
}

pub fn export_context_for_symbol(
    db: &Database,
    sym_name: &str,
    req: &ExportRequest,
) -> Result<ContextBundle> {
    let defs = db.find_symbols(sym_name, None)?;
    if defs.is_empty() {
        return Ok(ContextBundle {
            found: false,
            target_description: format!("symbol: {sym_name} (not found)"),
            primary: Vec::new(),
            related: Vec::new(),
            symbol_definitions: Vec::new(),
        });
    }

    // A zero limit would otherwise return `found: true` with empty
    // everything — the symbol resolved, but every `take(0)`/cap below drops
    // it again. `feature_bundle` normalizes 0 to 5; do the same here so both
    // entry points agree on what "no limit given" means.
    let limit = if req.limit == 0 { 5 } else { req.limit };
    let defs: Vec<Symbol> = defs.into_iter().take(limit).collect();
    let mut primary: Vec<SearchResult> = Vec::new();
    let mut primary_keys: HashSet<(String, u32)> = HashSet::new();
    for def in &defs {
        if primary.len() >= limit {
            break;
        }
        add_related_from_file(
            db,
            &def.file_path,
            def.line_start,
            &mut primary,
            &mut primary_keys,
        )?;
    }

    let mut related: Vec<SearchResult> = Vec::new();
    let mut related_keys: HashSet<(String, u32)> = primary_keys.clone();

    if req.include_callers || req.include_callees {
        add_related_for_symbols(
            db,
            &defs,
            req.include_callers,
            req.include_callees,
            limit,
            &mut related,
            &mut related_keys,
        )?;
    }

    Ok(finalize_bundle(ContextBundle {
        found: true,
        target_description: format!("symbol: {sym_name}"),
        primary,
        related,
        symbol_definitions: defs,
    }))
}

/// Build a curated [`ContextBundle`] for one feature_id. Composes the
/// feature's already-curated file list (entry + owned + tests + context)
/// with the existing chunk store and symbol graph, so an agent doesn't
/// have to fan out per-file `Read` calls after `find_feature` / `list_features`.
///
/// Layout:
/// - `primary[]` — chunks from owned + entry files, capped at `limit`.
/// - `related[]` — up to two requested caller/callee chunks, then chunks
///   from tests and context files. The combined list is capped by `limit`.
/// - `symbol_definitions[]` — entry-symbol definition (when present) +
///   any symbol definitions discovered while building primary chunks.
/// - `target_description` — `"feature: <title> (<feature_id>)"`.
///
/// When the feature_id doesn't resolve, returns an empty bundle with
/// `found=false` and a `not found` marker in `target_description` (mirrors
/// `export_context_for_symbol`'s missing-symbol behavior).
pub fn feature_bundle(
    db: &Database,
    feature_id: &str,
    include_callers: bool,
    include_callees: bool,
    limit: usize,
) -> Result<ContextBundle> {
    use codesage_protocol::FeatureFileRole;
    let limit = if limit == 0 { 5 } else { limit };

    let feature = match db.load_feature(feature_id)? {
        Some(f) => f,
        None => {
            return Ok(ContextBundle {
                found: false,
                target_description: format!("feature: {feature_id} (not found)"),
                primary: Vec::new(),
                related: Vec::new(),
                symbol_definitions: Vec::new(),
            });
        }
    };

    let mut primary: Vec<SearchResult> = Vec::new();
    let mut primary_keys: HashSet<(String, u32)> = HashSet::new();
    // Entry first so it's the first chunk in primary order. For the entry
    // file itself, prefer the chunk overlapping the feature's entry symbol
    // — `crates/cli/src/main.rs` opens with 400 lines of `use` statements
    // before `fn main()` starts, and an agent reviewing the feature wants
    // the body, not the imports. Fall back to first-chunk when the entry
    // symbol can't be located (no entry_symbol on the feature, or symbol
    // line is outside any chunk).
    let entry_line = feature.entry_symbol.as_ref().and_then(|sym| {
        entry_symbol_line(db, sym, &feature.entry_path)
            .ok()
            .flatten()
    });
    for f in feature
        .files
        .iter()
        .filter(|f| matches!(f.role, FeatureFileRole::Entry | FeatureFileRole::Owned))
    {
        if primary.len() >= limit {
            break;
        }
        if f.role == FeatureFileRole::Entry
            && let Some(line) = entry_line
        {
            add_chunk_at_line(db, &f.path, line, &mut primary, &mut primary_keys)?;
            // Fall back to first-chunk only when the symbol-overlap path
            // produced nothing (no chunk covers that line yet).
            if primary.is_empty() {
                add_first_chunk_of_file(db, &f.path, &mut primary, &mut primary_keys)?;
            }
        } else {
            add_first_chunk_of_file(db, &f.path, &mut primary, &mut primary_keys)?;
        }
    }

    let mut related: Vec<SearchResult> = Vec::new();
    let mut related_keys: HashSet<(String, u32)> = primary_keys.clone();

    // Symbol definitions: entry symbol (if any) + symbols overlapping the
    // primary chunks (annotated already by add_first_chunk_of_file).
    // Filter entry-symbol matches to definitions that live in the
    // feature's entry file when possible — `main` is a common-enough
    // name that an unqualified lookup pulls in unrelated definitions
    // (e.g. Python `if __name__ == "__main__"` modules share the same
    // entry_symbol = "main" string as Rust binaries).
    let mut symbol_definitions: Vec<Symbol> = Vec::new();
    let mut seen_sym: HashSet<String> = HashSet::new();
    if let Some(entry_sym) = &feature.entry_symbol {
        let all_defs = db.find_symbols(entry_sym, None)?;
        let in_entry_file: Vec<Symbol> = all_defs
            .iter()
            .filter(|d| d.file_path == feature.entry_path)
            .cloned()
            .collect();
        let preferred = if in_entry_file.is_empty() {
            all_defs
        } else {
            in_entry_file
        };
        for def in preferred {
            if seen_sym.insert(def.qualified_name.clone()) {
                symbol_definitions.push(def);
            }
        }
    }
    for r in &primary {
        for sum in &r.symbols {
            if !seen_sym.insert(sum.qualified_name.clone()) {
                continue;
            }
            if let Some(d) = find_definition_for_summary(db, sum, &r.file_path)? {
                symbol_definitions.push(d);
            }
        }
    }

    // Caller/callee expansion of the entry symbol when requested.
    if (include_callers || include_callees) && !symbol_definitions.is_empty() {
        // Use the entry symbol's definition(s) as the anchor — limited to
        // the first few so tests still fit inside the existing related cap.
        let anchors: Vec<Symbol> = symbol_definitions.iter().take(3).cloned().collect();
        add_related_for_symbols(
            db,
            &anchors,
            include_callers,
            include_callees,
            limit.min(2),
            &mut related,
            &mut related_keys,
        )?;
    }

    // Backfill tests (run-this-after-review signal), then context. Expansion
    // goes first only when explicitly requested; without graph candidates,
    // tests and context retain the full related budget.
    for role in [FeatureFileRole::Test, FeatureFileRole::Context] {
        for f in feature.files.iter().filter(|f| f.role == role) {
            if related.len() >= limit {
                break;
            }
            add_first_chunk_of_file(db, &f.path, &mut related, &mut related_keys)?;
        }
        if related.len() >= limit {
            break;
        }
    }

    Ok(finalize_bundle(ContextBundle {
        found: true,
        target_description: format!("feature: {} ({})", feature.title, feature.feature_id),
        primary,
        related,
        symbol_definitions,
    }))
}

/// Insert the chunk of `file_path` whose `[start_line, end_line]` covers
/// `line` (the entry symbol's `line_start`). Falls back silently when no
/// chunk covers that line — the outer caller then drops back to
/// `add_first_chunk_of_file`. Mirrors `add_related_from_file`'s
/// covering-chunk lookup but stays in the primary-chunk track for
/// feature_bundle.
fn add_chunk_at_line(
    db: &Database,
    file_path: &str,
    line: u32,
    out: &mut Vec<SearchResult>,
    seen: &mut HashSet<(String, u32)>,
) -> Result<()> {
    let chunks = db.chunks_for_file(file_path)?;
    let Some(c) = chunks
        .into_iter()
        .find(|c| c.start_line <= line && c.end_line >= line)
    else {
        return Ok(());
    };
    let key = (c.file_path.clone(), c.start_line);
    if !seen.insert(key) {
        return Ok(());
    }
    let mut result = SearchResult {
        file_path: c.file_path,
        language: parse_db_language(&c.language),
        content: c.content,
        start_line: c.start_line,
        end_line: c.end_line,
        score: 0.0,
        symbols: Vec::new(),
    };
    annotate_with_symbols(db, std::slice::from_mut(&mut result))?;
    out.push(result);
    Ok(())
}

/// Resolve the `line_start` of `entry_symbol` in `entry_path`. Used by
/// `feature_bundle` to pick the chunk that holds the entry symbol's
/// definition rather than the file's first chunk (which is usually
/// imports/use statements). Returns `Ok(None)` when the symbol can't be
/// uniquely placed inside the entry file — the caller falls back to
/// first-chunk lookup.
fn entry_symbol_line(db: &Database, entry_symbol: &str, entry_path: &str) -> Result<Option<u32>> {
    let defs = db.find_symbols(entry_symbol, None)?;
    Ok(defs
        .into_iter()
        .find(|d| d.file_path == entry_path)
        .map(|d| d.line_start))
}

/// Insert the first chunk of `file_path` into `out` (deduped by
/// `(path, start_line)`). Skips files that haven't been semantically
/// indexed yet — their chunks just don't exist. Used by `feature_bundle`
/// where we always want a deterministic per-file entry, not best-match
/// search semantics.
fn add_first_chunk_of_file(
    db: &Database,
    file_path: &str,
    out: &mut Vec<SearchResult>,
    seen: &mut HashSet<(String, u32)>,
) -> Result<()> {
    let chunks = db.chunks_for_file(file_path)?;
    let Some(c) = chunks.into_iter().min_by_key(|c| c.start_line) else {
        return Ok(());
    };
    let key = (c.file_path.clone(), c.start_line);
    if !seen.insert(key) {
        return Ok(());
    }
    let mut result = SearchResult {
        file_path: c.file_path,
        language: parse_db_language(&c.language),
        content: c.content,
        start_line: c.start_line,
        end_line: c.end_line,
        score: 0.0,
        symbols: Vec::new(),
    };
    annotate_with_symbols(db, std::slice::from_mut(&mut result))?;
    out.push(result);
    Ok(())
}

fn add_related_for_symbols(
    db: &Database,
    symbols: &[Symbol],
    include_callers: bool,
    include_callees: bool,
    limit: usize,
    related: &mut Vec<SearchResult>,
    related_keys: &mut HashSet<(String, u32)>,
) -> Result<()> {
    let mut seen_callee_defs: HashSet<(String, String, u32)> = HashSet::new();
    for sym in symbols {
        if include_callers {
            add_callers_for_symbol(db, sym, limit, related, related_keys)?;
        }
        if related.len() >= limit {
            break;
        }
        if include_callees {
            add_callees_for_symbol(db, sym, limit, related, related_keys, &mut seen_callee_defs)?;
        }
        if related.len() >= limit {
            break;
        }
    }
    Ok(())
}

fn add_callers_for_symbol(
    db: &Database,
    sym: &Symbol,
    limit: usize,
    related: &mut Vec<SearchResult>,
    related_keys: &mut HashSet<(String, u32)>,
) -> Result<()> {
    let refs = references_for_symbol(db, sym)?;
    for r in refs {
        if related.len() >= limit {
            break;
        }
        add_related_from_file(db, &r.from_file, r.line, related, related_keys)?;
    }
    Ok(())
}

fn add_callees_for_symbol(
    db: &Database,
    sym: &Symbol,
    limit: usize,
    related: &mut Vec<SearchResult>,
    related_keys: &mut HashSet<(String, u32)>,
    seen_defs: &mut HashSet<(String, String, u32)>,
) -> Result<()> {
    let refs = db.references_in_file_range(&sym.file_path, sym.line_start, sym.line_end)?;
    for r in refs {
        if related.len() >= limit {
            break;
        }
        if !is_callee_reference(r.kind) {
            continue;
        }
        for def in resolve_callee_definitions(db, &sym.file_path, &r.to_name)? {
            let key = (
                def.file_path.clone(),
                def.qualified_name.clone(),
                def.line_start,
            );
            if !seen_defs.insert(key) {
                continue;
            }
            add_related_from_file(db, &def.file_path, def.line_start, related, related_keys)?;
            if related.len() >= limit {
                break;
            }
        }
    }
    Ok(())
}

/// Case-insensitive qualified-name equality for the second tier of
/// [`resolve_callee_definitions`]. ASCII-only by design: symbol spellings
/// that differ beyond ASCII case are different symbols, not spelling
/// variants, and must fall through to the import-evidence filter instead.
fn qualified_name_matches_normalized(sym: &Symbol, to_name: &str) -> bool {
    sym.qualified_name.eq_ignore_ascii_case(to_name) || sym.name.eq_ignore_ascii_case(to_name)
}

pub(crate) fn resolve_callee_definitions(
    db: &Database,
    caller_file: &str,
    to_name: &str,
) -> Result<Vec<Symbol>> {
    let candidates = db.find_symbols(to_name, None)?;
    if is_qualified_symbol_name(to_name) {
        // Exact spelling first, preserving current behavior wherever the
        // callsite spells the name exactly as indexed.
        let exact: Vec<Symbol> = candidates
            .iter()
            .filter(|s| s.qualified_name == to_name || s.name == to_name)
            .cloned()
            .collect();
        if !exact.is_empty() {
            return Ok(exact);
        }
        // Normalized pass for case-only mismatches (PHP class names are
        // case-insensitive; barrels re-export under a different case). When
        // even that finds nothing, fall through to the import-evidence
        // filter below instead of returning empty: a qualified callsite
        // whose spelling matches neither form previously dropped every
        // reverse edge for the symbol.
        let folded: Vec<Symbol> = candidates
            .iter()
            .filter(|s| qualified_name_matches_normalized(s, to_name))
            .cloned()
            .collect();
        if !folded.is_empty() {
            return Ok(folded);
        }
    }
    if candidates.len() <= 1 {
        return Ok(candidates);
    }
    let import_refs = import_refs_for_file(db, caller_file)?;

    // A definition in the caller's own file is a valid target: a same-file
    // call emits no import edge, so without this the local
    // definition is filtered out and the reverse edge (read by
    // `impact_analysis` / `assess_risk` through `references_for_symbol`) is
    // lost. Mirrors the same-file preference in `resolve_def_summary`.
    let is_local = |s: &Symbol| s.file_path == caller_file;

    // One filter over both kinds of evidence. Letting path specifiers win
    // outright was tried and removed: it measured identically on axios and
    // monolog (F1 0.61 / 0.79 either way) while carrying a real failure mode,
    // because `import_refs_for_file` pools every specifier in the file without
    // recording which binding each introduced — so an unrelated import could
    // match a same-named candidate and suppress the correct one. Dropping a
    // real dependent is the unsafe direction for a what-to-review signal;
    // over-inclusion is not.
    let filtered: Vec<Symbol> = candidates
        .into_iter()
        .filter(|s| {
            is_local(s)
                || import_refs
                    .iter()
                    .any(|imp| import_ref_targets_symbol(imp, caller_file, to_name, s))
        })
        .collect();
    Ok(filtered)
}

// Covers everything `list_file_dependencies().imports` would add: that list
// is DISTINCT to_name over the same file's refs restricted to import/include,
// a strict subset of the import/include/trait_use filter here — so the
// expensive imported_by half of that call is never needed on this path.
fn import_refs_for_file(db: &Database, caller_file: &str) -> Result<Vec<String>> {
    let mut refs = Vec::new();
    if let Some(file_id) = db.file_id_for_path(caller_file)? {
        for (to_name, kind) in db.refs_outgoing_for_file_id(file_id)? {
            // `ImportBinding` is safe to admit here only because the caller
            // consults path evidence first: a bare binding name matches every
            // same-named candidate, so it must never compete with a specifier
            // that identifies one. As pure fallback it recovers the files whose
            // specifier points at a re-exporting barrel rather than at the
            // defining module.
            if matches!(
                kind,
                ReferenceKind::Import
                    | ReferenceKind::ImportBinding
                    | ReferenceKind::Include
                    | ReferenceKind::TraitUse
            ) {
                refs.push(to_name);
            }
        }
    }
    refs.sort();
    refs.dedup();
    Ok(refs)
}

/// True when `import_ref` (e.g. `crate::helpers_a::helper`) names `sym` in
/// `sym_file` even if the symbol table only stores the bare tail (`helper`).
fn import_ref_targets_symbol(
    import_ref: &str,
    caller_file: &str,
    callee_name: &str,
    sym: &Symbol,
) -> bool {
    if import_ref == sym.qualified_name || import_ref == sym.name {
        return true;
    }
    // Path-style specifiers name a file, not a symbol: JS/TS records
    // `import X from './headers.js'` as `./headers.js`, C/C++ records
    // `#include "dir/foo.h"` as `dir/foo.h`. Neither can ever equal a symbol
    // name, so before this branch every candidate was filtered out and any
    // symbol with two same-named definitions lost all its reverse edges. In
    // JS/TS that is the common case, not a corner: a `.d.ts` declaration
    // beside its `.js` implementation gives two definitions of one name.
    if is_path_specifier(import_ref) {
        return import_path_targets_file(import_ref, caller_file, &sym.file_path);
    }
    let Some((module, tail)) = import_ref.rsplit_once("::") else {
        return import_ref == callee_name && import_ref == sym.name;
    };
    if tail != callee_name && tail != sym.name {
        return false;
    }
    rust_module_candidates(module, caller_file)
        .iter()
        .any(|c| c == &sym.file_path)
}

/// File paths a Rust `use` module path may resolve to. `crate::` is relative
/// to the importer's own crate root: when the importer path shows a `src/`
/// root, resolve against that root ONLY — adding the generic repo-root
/// guesses alongside would let `crates/a/src/lib.rs` claim the root crate's
/// `src/util.rs`. The generic layouts remain the fallback for importers with
/// no derivable src root (e.g. a root-level `lib.rs`).
fn rust_module_candidates(module: &str, importer_file: &str) -> Vec<String> {
    let module = module.strip_prefix("crate::").unwrap_or(module);
    if module.is_empty() || module == "crate" {
        return Vec::new();
    }
    let module_path = module.replace("::", "/");
    if let Some(src_root) = importer_src_root(importer_file) {
        return vec![
            format!("{src_root}{module_path}.rs"),
            format!("{src_root}{module_path}/mod.rs"),
        ];
    }
    vec![
        format!("{module_path}.rs"),
        format!("{module_path}/mod.rs"),
        format!("src/{module_path}.rs"),
        format!("src/{module_path}/mod.rs"),
    ]
}

/// The `src/` root the importer lives under, trailing slash included:
/// `crates/app/src/lib.rs` → `crates/app/src/`, `src/lib.rs` → `src/`.
fn importer_src_root(importer_file: &str) -> Option<&str> {
    if let Some(idx) = importer_file.rfind("/src/") {
        return Some(&importer_file[..idx + "/src/".len()]);
    }
    importer_file.strip_prefix("src/").map(|_| "src/")
}

/// File-level counterpart of [`import_ref_targets_symbol`]: true when the
/// import/include ref recorded in `importer_file` resolves to `target_file`.
/// Backs `list_dependencies`' `imported_by`, whose SQL half only joins refs
/// that name a symbol — path specifiers (`./util.js`, `dir/foo.h`) and Rust
/// `use crate::…` module paths name a file or module and never join.
pub(crate) fn import_ref_targets_file(
    import_ref: &str,
    importer_file: &str,
    target_file: &str,
) -> bool {
    if import_ref.starts_with("./") || import_ref.starts_with("../") {
        return import_path_targets_file(import_ref, importer_file, target_file);
    }
    if import_ref.contains("::") {
        // `use crate::util` names util.rs directly; `use crate::util::helper`
        // names it through the parent of the imported item. External paths
        // (`std::io::Read`) derive candidates that match no indexed file.
        if rust_module_candidates(import_ref, importer_file)
            .iter()
            .any(|c| c == target_file)
        {
            return true;
        }
        return import_ref.rsplit_once("::").is_some_and(|(module, _)| {
            rust_module_candidates(module, importer_file)
                .iter()
                .any(|c| c == target_file)
        });
    }
    // Quoted-include style: `util.h` or `sub/foo.h`. Resolve against the
    // includer's directory, then the project root — both exact. No stem or
    // suffix match: `sub/foo.h` must not claim every `*/sub/foo.h` in the
    // project. Includes reached through other `-I` paths stay unresolved.
    if import_ref.contains('/') || import_ref.contains('.') {
        let base = importer_file.rsplit_once('/').map_or("", |(dir, _)| dir);
        if lexical_join(base, import_ref).is_some_and(|resolved| resolved == target_file) {
            return true;
        }
        return import_ref == target_file;
    }
    false
}

fn is_path_specifier(s: &str) -> bool {
    s.starts_with("./") || s.starts_with("../") || s.contains('/')
}

/// Extensions a path specifier may omit or misname. TypeScript ESM is the
/// reason `.js` maps to `.ts`: the spec requires the *emitted* extension in the
/// specifier, so `./foo.js` routinely refers to `foo.ts` on disk. The same
/// swap covers the explicit module flavors: `./foo.mjs` for `foo.mts` and
/// `./foo.cjs` for `foo.cts` on disk.
const IMPORT_EXTENSIONS: [&str; 12] = [
    "js", "mjs", "cjs", "jsx", "ts", "tsx", "mts", "cts", "d.ts", "h", "hpp", "py",
];

fn import_path_targets_file(spec: &str, caller_file: &str, sym_file: &str) -> bool {
    if spec.starts_with("./") || spec.starts_with("../") {
        let base = caller_file.rsplit_once('/').map_or("", |(dir, _)| dir);
        return match lexical_join(base, spec) {
            Some(resolved) => relative_file_matches(&resolved, sym_file),
            None => false,
        };
    }
    // Non-relative specifiers are one of two things: a C include, which names a
    // real file and always carries its extension, or a package path, which
    // names nothing in this repo. So match exactly or on a separator-anchored
    // suffix, and never guess an extension — otherwise the npm specifier
    // `pkg/sub` claims the unrelated project file `pkg/sub.ts`, and a Go dot
    // import of `github.com/x/y` claims `github.com/x/y.ts`. The separator
    // anchor is what stops `net/utils.h` from claiming `net_utils.h`.
    spec == sym_file || sym_file.ends_with(&format!("/{spec}"))
}

/// Joins `spec` onto `base`, resolving `.` and `..` textually. Returns `None`
/// when the specifier escapes above the project root, which cannot name an
/// indexed file.
fn lexical_join(base: &str, spec: &str) -> Option<String> {
    let mut parts: Vec<&str> = if base.is_empty() {
        Vec::new()
    } else {
        base.split('/').collect()
    };
    for seg in spec.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

fn relative_file_matches(resolved: &str, sym_file: &str) -> bool {
    if resolved == sym_file {
        return true;
    }
    // Look for the extension in the last path segment only. A specifier like
    // `./dir.v1/foo` has its last dot in the *directory* part, and splitting on
    // that yields the stem `dir`, which then matches an unrelated `dir.ts`.
    let segment_start = resolved.rfind('/').map_or(0, |i| i + 1);
    match resolved[segment_start..].rfind('.') {
        // The specifier carries an extension. It may still differ from the file
        // on disk — TypeScript ESM requires the emitted `.js` for a `.ts`
        // source — so swap it. Do not also append, or `./foo.js` claims
        // `foo.js.ts`.
        Some(dot) => {
            let stem = &resolved[..segment_start + dot];
            IMPORT_EXTENSIONS
                .iter()
                .any(|ext| sym_file == format!("{stem}.{ext}"))
        }
        // No extension: `./foo` -> `foo.ts`, or the directory-index form
        // `./utils` -> `utils/index.js`.
        None => IMPORT_EXTENSIONS.iter().any(|ext| {
            sym_file == format!("{resolved}.{ext}") || sym_file == format!("{resolved}/index.{ext}")
        }),
    }
}

pub(crate) fn is_callee_reference(kind: ReferenceKind) -> bool {
    matches!(
        kind,
        ReferenceKind::Call
            | ReferenceKind::Instantiation
            | ReferenceKind::Import
            | ReferenceKind::ImportBinding
            | ReferenceKind::Include
            | ReferenceKind::Inheritance
            | ReferenceKind::TraitUse
            | ReferenceKind::TypeHint
    )
}

fn add_related_from_file(
    db: &Database,
    file_path: &str,
    line: u32,
    out: &mut Vec<SearchResult>,
    seen: &mut HashSet<(String, u32)>,
) -> Result<()> {
    let chunks = db.chunks_for_file(file_path)?;
    let best = chunks
        .into_iter()
        .find(|c| c.start_line <= line && c.end_line >= line);
    if let Some(c) = best {
        let key = (c.file_path.clone(), c.start_line);
        if seen.insert(key) {
            let mut result = SearchResult {
                file_path: c.file_path,
                language: parse_db_language(&c.language),
                content: c.content,
                start_line: c.start_line,
                end_line: c.end_line,
                score: 0.0,
                symbols: Vec::new(),
            };
            annotate_with_symbols(db, std::slice::from_mut(&mut result))?;
            out.push(result);
        }
    }
    Ok(())
}

#[cfg(test)]
mod import_path_tests {
    use super::*;

    #[test]
    fn relative_specifier_resolves_against_the_caller_directory() {
        assert!(import_path_targets_file(
            "./headers.js",
            "lib/core/client.js",
            "lib/core/headers.js"
        ));
        assert!(import_path_targets_file(
            "../helpers/util.js",
            "lib/core/client.js",
            "lib/helpers/util.js"
        ));
        // Same basename in a different directory is not the same file.
        assert!(!import_path_targets_file(
            "./headers.js",
            "lib/core/client.js",
            "lib/other/headers.js"
        ));
    }

    #[test]
    fn specifier_extension_may_differ_from_the_file_on_disk() {
        // TypeScript ESM requires the emitted extension in the specifier.
        assert!(import_path_targets_file(
            "./foo.js",
            "src/client.ts",
            "src/foo.ts"
        ));
        // Omitted entirely, and the directory-index form.
        assert!(import_path_targets_file(
            "./foo",
            "src/client.ts",
            "src/foo.tsx"
        ));
        assert!(import_path_targets_file(
            "./utils",
            "src/client.js",
            "src/utils/index.js"
        ));
    }

    #[test]
    fn mts_cts_extensions_resolve_like_their_emit_targets() {
        // `.mts`/`.cts` are indexed as TypeScript; the extension swap must
        // admit them both bare and under their emitted `.mjs`/`.cjs` names.
        assert!(import_path_targets_file(
            "./foo",
            "src/client.ts",
            "src/foo.mts"
        ));
        assert!(import_path_targets_file(
            "./foo",
            "src/client.ts",
            "src/foo.cts"
        ));
        assert!(import_path_targets_file(
            "./foo.mjs",
            "src/client.ts",
            "src/foo.mts"
        ));
        assert!(import_path_targets_file(
            "./foo.cjs",
            "src/client.ts",
            "src/foo.cts"
        ));
        // The directory-index form applies to them as well.
        assert!(import_path_targets_file(
            "./utils",
            "src/client.ts",
            "src/utils/index.mts"
        ));
    }

    #[test]
    fn go_import_path_does_not_match_an_unrelated_file() {
        // The last dot of `github.com/x/y` sits in the directory part. Splitting
        // the extension over the whole path produced the stem `github`, which
        // matched any same-named source file elsewhere in a mixed repo.
        assert!(!import_path_targets_file(
            "github.com/x/y",
            "cmd/app/main.go",
            "github.py"
        ));
        assert!(!import_path_targets_file(
            "github.com/x/y",
            "cmd/app/main.go",
            "github.h"
        ));
    }

    #[test]
    fn escaping_above_the_root_resolves_to_nothing() {
        assert!(!import_path_targets_file(
            "../../x.js",
            "a/client.js",
            "x.js"
        ));
        // The same specifier from two directories deep is in range.
        assert!(import_path_targets_file(
            "../../x.js",
            "a/b/client.js",
            "x.js"
        ));
    }

    #[test]
    fn declaration_files_resolve_through_both_branches() {
        // Specifier carries no extension, so one is appended.
        assert!(import_path_targets_file(
            "./foo",
            "src/client.ts",
            "src/foo.d.ts"
        ));
        // Specifier carries `.js`, which is swapped.
        assert!(import_path_targets_file(
            "./foo.js",
            "src/client.ts",
            "src/foo.d.ts"
        ));
    }

    #[test]
    fn specifier_with_an_extension_is_not_also_appended_to() {
        // `./foo.js` must not claim `foo.js.ts`.
        assert!(!import_path_targets_file(
            "./foo.js",
            "src/client.ts",
            "src/foo.js.ts"
        ));
    }

    #[test]
    fn rust_use_paths_resolve_to_module_files() {
        // Item import: the parent module names the file.
        assert!(import_ref_targets_file(
            "crate::util::helper",
            "src/lib.rs",
            "src/util.rs"
        ));
        assert!(import_ref_targets_file(
            "crate::util::helper",
            "lib.rs",
            "util/mod.rs"
        ));
        // Whole-module import: the path itself names the file.
        assert!(import_ref_targets_file(
            "crate::util",
            "src/lib.rs",
            "src/util.rs"
        ));
        // Workspace member: `crate::` is the importer's own src root.
        assert!(import_ref_targets_file(
            "crate::util::helper",
            "crates/app/src/lib.rs",
            "crates/app/src/util.rs"
        ));
        // A sibling crate's same-named module is not in `crate::`. The
        // repo-root `src/` guess cannot fire here (no such path), and the
        // src-root candidate is derived from the importer.
        assert!(!import_ref_targets_file(
            "crate::util::helper",
            "crates/app/src/lib.rs",
            "crates/other/src/util.rs"
        ));
        // An importer with its own src root must not claim the ROOT crate's
        // module either: the generic `src/` guess is a fallback, not an
        // always-on candidate.
        assert!(!import_ref_targets_file(
            "crate::util::helper",
            "crates/app/src/lib.rs",
            "src/util.rs"
        ));
        // External paths derive candidates that match no indexed file.
        assert!(!import_ref_targets_file(
            "std::io::Read",
            "src/lib.rs",
            "src/io.rs"
        ));
    }

    #[test]
    fn bare_includes_resolve_against_the_includer_directory() {
        assert!(import_ref_targets_file(
            "util.h",
            "src/main.c",
            "src/util.h"
        ));
        assert!(import_ref_targets_file("util.h", "main.c", "util.h"));
        // Exact join only: no claiming a same-named header elsewhere.
        assert!(!import_ref_targets_file(
            "util.h",
            "src/main.c",
            "other/util.h"
        ));
        // A bare symbol name is not a file.
        assert!(!import_ref_targets_file("util", "main.py", "util.py"));
    }

    #[test]
    fn directory_includes_resolve_exactly_never_by_suffix() {
        // Includer-directory join.
        assert!(import_ref_targets_file(
            "sub/foo.h",
            "app/main.c",
            "app/sub/foo.h"
        ));
        // Project-root join (`-I.` style include).
        assert!(import_ref_targets_file(
            "sub/foo.h",
            "app/main.c",
            "sub/foo.h"
        ));
        // No suffix match: an unrelated tree's `*/sub/foo.h` stays unclaimed.
        assert!(!import_ref_targets_file(
            "sub/foo.h",
            "app/main.c",
            "vendor/sub/foo.h"
        ));
    }

    #[test]
    fn path_specifiers_route_through_the_relative_matcher() {
        assert!(import_ref_targets_file(
            "./util.js",
            "lib/main.js",
            "lib/util.js"
        ));
        assert!(!import_ref_targets_file(
            "./util.js",
            "lib/main.js",
            "other/util.js"
        ));
    }

    #[test]
    fn a_dot_in_a_directory_component_is_not_an_extension() {
        // Stem must come from the last segment: `dir.v1/foo` is not `dir`.
        assert!(!import_path_targets_file(
            "./dir.v1/foo",
            "src/client.ts",
            "src/dir.ts"
        ));
        assert!(import_path_targets_file(
            "./dir.v1/foo",
            "src/client.ts",
            "src/dir.v1/foo.ts"
        ));
    }

    #[test]
    fn package_specifiers_do_not_claim_same_named_project_files() {
        // An npm subpath import, not a relative path to `pkg/sub.ts`.
        assert!(!import_path_targets_file(
            "pkg/sub",
            "src/client.ts",
            "pkg/sub.ts"
        ));
        // A Go dot import names a package directory, not a source file.
        assert!(!import_path_targets_file(
            "github.com/x/y",
            "cmd/app/main.go",
            "github.com/x/y.ts"
        ));
    }

    #[test]
    fn non_relative_include_matches_on_a_separator_boundary() {
        assert!(import_path_targets_file(
            "net/utils.h",
            "src/main.c",
            "include/net/utils.h"
        ));
        // `utils.h` must not be claimed by `net_utils.h`.
        assert!(!import_path_targets_file(
            "net/utils.h",
            "src/main.c",
            "include/net_utils.h"
        ));
    }
}

#[cfg(test)]
mod line_number_tests {
    use super::*;

    #[test]
    fn numbers_body_after_augmentation_header() {
        let content = "# app/Svc.php\n# App\\Svc (class)\n<?php\nclass Svc {}";
        let out = number_chunk_lines(content, "app/Svc.php", 10);
        let lines: Vec<&str> = out.lines().collect();
        // Header lines pass through unnumbered.
        assert_eq!(lines[0], "# app/Svc.php");
        assert_eq!(lines[1], "# App\\Svc (class)");
        // Body numbered from start_line.
        assert_eq!(lines[2], "  10 | <?php");
        assert_eq!(lines[3], "  11 | class Svc {}");
    }

    #[test]
    fn numbers_from_line_one_when_no_header() {
        // C/Rust chunks are not augmented — first line is real source.
        let content = "fn main() {\n    let x = 1;\n}";
        let out = number_chunk_lines(content, "src/main.rs", 42);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "  42 | fn main() {");
        assert_eq!(lines[1], "  43 |     let x = 1;");
        assert_eq!(lines[2], "  44 | }");
    }

    #[test]
    fn source_comment_resembling_header_is_not_stripped() {
        // Python body whose first line is a `# word (word)` comment must
        // not be mistaken for a symbol-header line: `(later)` is not a
        // SymbolKind, so numbering still starts at the comment.
        let content = "# app/x.py\n# cleanup (later)\nx = 1";
        let out = number_chunk_lines(content, "app/x.py", 5);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "# app/x.py");
        assert_eq!(lines[1], "   5 | # cleanup (later)");
        assert_eq!(lines[2], "   6 | x = 1");
    }

    #[test]
    fn header_anchor_must_match_this_files_path() {
        // A `# something` first line that is not THIS chunk's path anchor
        // is treated as body, not header.
        let content = "# not a path\ncode";
        let out = number_chunk_lines(content, "app/x.py", 1);
        assert_eq!(out.lines().next().unwrap(), "   1 | # not a path");
    }

    #[test]
    fn preserves_trailing_newline_shape() {
        assert!(number_chunk_lines("a\nb\n", "f.rs", 1).ends_with("b\n"));
        assert!(!number_chunk_lines("a\nb", "f.rs", 1).ends_with('\n'));
        assert_eq!(number_chunk_lines("", "f.rs", 1), "");
    }

    #[test]
    fn is_symbol_header_line_matches_known_kinds_only() {
        assert!(is_symbol_header_line("# App\\Svc (class)"));
        assert!(is_symbol_header_line("# foo (function)"));
        assert!(!is_symbol_header_line("# cleanup (later)"));
        assert!(!is_symbol_header_line("not a comment"));
        assert!(!is_symbol_header_line("# no parens here"));
    }
}

#[cfg(test)]
mod context_export_tests {
    use super::*;
    use codesage_protocol::{FileInfo, Language, Reference, ReferenceKind, SymbolKind};

    fn symbol(name: &str, qualified_name: &str, file_path: &str) -> Symbol {
        Symbol {
            name: name.to_string(),
            qualified_name: qualified_name.to_string(),
            kind: SymbolKind::Method,
            file_path: file_path.to_string(),
            line_start: 1,
            line_end: 1,
            col_start: 0,
            col_end: 0,
            rationale: vec![],
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
        }
    }

    #[test]
    fn import_refs_for_file_covers_list_file_dependencies_imports() {
        let db = Database::open_in_memory().unwrap();
        let file_id = db
            .upsert_file(&FileInfo {
                path: "app/a.php".to_string(),
                language: Language::Php,
                content_hash: "a".to_string(),
            })
            .unwrap();
        let mk = |to: &str, kind| Reference {
            from_file: "app/a.php".to_string(),
            from_symbol: None,
            to_name: to.to_string(),
            kind,
            line: 1,
            col: 0,
        };
        db.insert_references(
            file_id,
            &[
                mk("App\\Imported", ReferenceKind::Import),
                mk("inc.php", ReferenceKind::Include),
                mk("App\\SomeTrait", ReferenceKind::TraitUse),
                mk("helper", ReferenceKind::Call),
                // Same name referenced again on another line: exercises the
                // by-name dedupe below. An identical (line, col) duplicate
                // would be rejected by the uq_refs_identity backstop.
                Reference {
                    line: 2,
                    ..mk("App\\Imported", ReferenceKind::Import)
                },
            ],
        )
        .unwrap();

        let refs = import_refs_for_file(&db, "app/a.php").unwrap();
        assert_eq!(
            refs,
            vec![
                "App\\Imported".to_string(),
                "App\\SomeTrait".to_string(),
                "inc.php".to_string(),
            ],
            "import/include/trait_use names, sorted and deduped; calls excluded"
        );

        // The imports half of list_file_dependencies must stay a subset of
        // this result — that is what lets import_refs_for_file skip the
        // expensive imported_by UNION that call also computes.
        let deps = db.list_file_dependencies("app/a.php").unwrap();
        assert!(!deps.imports.is_empty(), "fixture must produce imports");
        for imp in &deps.imports {
            assert!(
                refs.contains(imp),
                "list_file_dependencies import {imp} missing from import_refs_for_file: {refs:?}"
            );
        }
    }

    #[test]
    fn import_refs_for_file_empty_for_unindexed_file() {
        let db = Database::open_in_memory().unwrap();
        let refs = import_refs_for_file(&db, "no/such/file.rs").unwrap();
        assert!(
            refs.is_empty(),
            "unindexed file must yield no imports: {refs:?}"
        );
    }

    #[test]
    fn qualified_symbol_without_exact_refs_does_not_fallback_to_bare_tail() {
        let db = Database::open_in_memory().unwrap();
        let repo_file = db
            .upsert_file(&FileInfo {
                path: "app/repo_controller.py".to_string(),
                language: Language::Python,
                content_hash: "repo".to_string(),
            })
            .unwrap();
        let cache_file = db
            .upsert_file(&FileInfo {
                path: "app/cache_controller.py".to_string(),
                language: Language::Python,
                content_hash: "cache".to_string(),
            })
            .unwrap();
        db.insert_references(repo_file, &[reference("find", "app/repo_controller.py")])
            .unwrap();
        db.insert_references(cache_file, &[reference("find", "app/cache_controller.py")])
            .unwrap();

        let refs = references_for_symbol(&db, &symbol("find", "Repository.find", "app/models.py"))
            .unwrap();

        assert!(
            refs.is_empty(),
            "qualified method without exact refs must not fall back to all bare `find` references: {refs:?}"
        );
    }

    #[test]
    fn same_file_definition_resolves_when_name_is_ambiguous() {
        // `helper` is defined in both a.rs and b.rs (ambiguous bare name). A call
        // to `helper` from a.rs has no import edge for the local definition, so
        // the import filter alone would drop it and lose the same-file reverse
        // edge. The same-file fallback keeps a.rs's definition; the unimported
        // homonym in b.rs is still excluded.
        let db = Database::open_in_memory().unwrap();
        let a = db
            .upsert_file(&FileInfo {
                path: "a.rs".to_string(),
                language: Language::Rust,
                content_hash: "a".to_string(),
            })
            .unwrap();
        let b = db
            .upsert_file(&FileInfo {
                path: "b.rs".to_string(),
                language: Language::Rust,
                content_hash: "b".to_string(),
            })
            .unwrap();
        db.insert_symbols(a, &[symbol("helper", "helper", "a.rs")])
            .unwrap();
        db.insert_symbols(b, &[symbol("helper", "helper", "b.rs")])
            .unwrap();

        let resolved = resolve_callee_definitions(&db, "a.rs", "helper").unwrap();
        assert!(
            resolved.iter().any(|s| s.file_path == "a.rs"),
            "same-file ambiguous call must resolve to the local definition: {resolved:?}"
        );
        assert!(
            !resolved.iter().any(|s| s.file_path == "b.rs"),
            "the unimported homonym must not be resolved: {resolved:?}"
        );
    }

    #[test]
    fn summary_definition_lookup_uses_qualified_name_and_file() {
        let db = Database::open_in_memory().unwrap();
        let cpp_file = db
            .upsert_file(&FileInfo {
                path: "fixtures/sample.cpp".to_string(),
                language: Language::Cpp,
                content_hash: "cpp".to_string(),
            })
            .unwrap();
        let rust_file = db
            .upsert_file(&FileInfo {
                path: "src/db.rs".to_string(),
                language: Language::Rust,
                content_hash: "rust".to_string(),
            })
            .unwrap();
        db.insert_symbols(
            cpp_file,
            &[symbol(
                "open",
                "app::net::Connection::open",
                "fixtures/sample.cpp",
            )],
        )
        .unwrap();
        db.insert_symbols(rust_file, &[symbol("open", "Database::open", "src/db.rs")])
            .unwrap();

        let summary = SymbolSummary {
            name: "open".to_string(),
            qualified_name: "Database::open".to_string(),
            kind: codesage_protocol::SymbolKind::Method,
        };
        let found = find_definition_for_summary(&db, &summary, "src/db.rs")
            .unwrap()
            .expect("definition should match the summary");

        assert_eq!(found.qualified_name, "Database::open");
        assert_eq!(found.file_path, "src/db.rs");
    }
}
