use std::path::Path;

use anyhow::{Context, Result};
use codesage_features::trust_boundary::derive_from_refs;
use codesage_protocol::{FileInfo, IndexStats, Reference, Symbol};
use codesage_storage::Database;
use codesage_storage::db::FingerprintInput;
use rayon::prelude::*;

use codesage_parser::discover::discover_files_with_excludes;
use codesage_parser::extract::extract_symbols;
use codesage_parser::fingerprint::{FunctionFingerprint, file_fingerprints};
use codesage_parser::parse::parse_file;
use codesage_parser::references::extract_references;

#[derive(Debug)]
struct ParsedFile {
    info: FileInfo,
    symbols: Vec<Symbol>,
    refs: Vec<Reference>,
    fingerprints: Vec<FunctionFingerprint>,
}

fn parse_one(root: &Path, file_info: &FileInfo) -> Result<ParsedFile> {
    let abs_path = root.join(&file_info.path);
    let source = std::fs::read(&abs_path).with_context(|| format!("reading {}", file_info.path))?;
    let tree = parse_file(&source, file_info.language)
        .with_context(|| format!("parsing {}", file_info.path))?;
    let symbols = extract_symbols(&tree, &source, file_info.language, &file_info.path)
        .with_context(|| format!("extracting symbols from {}", file_info.path))?;
    let mut refs = extract_references(&tree, &source, file_info.language, &file_info.path)
        .with_context(|| format!("extracting references from {}", file_info.path))?;
    populate_from_symbol(&symbols, &mut refs);
    let fingerprints = file_fingerprints(&tree, &source, file_info.language);
    Ok(ParsedFile {
        info: file_info.clone(),
        symbols,
        refs,
        fingerprints,
    })
}

/// Map parsed fingerprints to borrowed insert rows.
fn fingerprint_inputs(p: &ParsedFile) -> Vec<FingerprintInput<'_>> {
    p.fingerprints
        .iter()
        .map(|f| FingerprintInput {
            name: &f.name,
            kind: f.kind.as_str(),
            line_start: f.line_start,
            line_end: f.line_end,
            leaf_count: f.leaf_count as u32,
            fp: &f.fp,
        })
        .collect()
}

/// Set each reference's `from_symbol` to the qualified name of the innermost
/// symbol whose line range encloses the reference. This is what lets
/// `find_references` report the calling symbol and `impact_analysis` walk the
/// call graph at symbol precision instead of re-deriving the caller from
/// `(file, line)`. References with no enclosing symbol (a top-level import, a
/// reference in a file with no extracted symbols) keep `from_symbol = None`,
/// so downstream consumers degrade cleanly.
fn populate_from_symbol(symbols: &[Symbol], refs: &mut [Reference]) {
    if symbols.is_empty() {
        return;
    }
    for r in refs.iter_mut() {
        let mut best: Option<&Symbol> = None;
        for s in symbols {
            if s.line_start <= r.line && r.line <= s.line_end {
                let span = s.line_end - s.line_start;
                match best {
                    // Strictly smaller span = more deeply nested; ties keep the
                    // first (outer-declared) match for determinism.
                    Some(b) if (b.line_end - b.line_start) <= span => {}
                    _ => best = Some(s),
                }
            }
        }
        if let Some(s) = best {
            r.from_symbol = Some(s.qualified_name.clone());
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexStrategy {
    Full,
    Incremental,
}

fn index(
    root: &Path,
    db: &Database,
    exclude_patterns: &[String],
    strategy: IndexStrategy,
    verbose: bool,
) -> Result<IndexStats> {
    let files = discover_files_with_excludes(root, exclude_patterns)?;
    let mut stats = IndexStats::default();

    if verbose {
        tracing::info!(total = files.len(), "discovered files for structural index");
    }

    let discovered_paths: std::collections::HashSet<&str> =
        files.iter().map(|f| f.path.as_str()).collect();
    let existing_paths = db.all_file_paths()?;
    let orphans: Vec<&str> = existing_paths
        .iter()
        .filter(|p| !discovered_paths.contains(p.as_str()))
        .map(|p| p.as_str())
        .collect();
    if !orphans.is_empty() {
        db.execute_batch(|db| {
            for path in &orphans {
                db.remove_file(path)?;
            }
            Ok(())
        })?;
        stats.files_removed = orphans.len();
    }

    let to_parse: Vec<&FileInfo> = match strategy {
        IndexStrategy::Full => files.iter().collect(),
        IndexStrategy::Incremental => {
            // One sequential scan of `files` instead of one SELECT per discovered file.
            let existing_hashes = db.all_file_hashes()?;
            files
                .iter()
                .filter(|f| existing_hashes.get(&f.path) != Some(&f.content_hash))
                .collect()
        }
    };
    if strategy == IndexStrategy::Incremental {
        stats.files_skipped = files.len() - to_parse.len();
    }

    if verbose {
        tracing::info!(files_to_parse = to_parse.len(), "parsing files");
    }

    let parsed_results: Vec<Result<ParsedFile>> =
        to_parse.par_iter().map(|f| parse_one(root, f)).collect();
    let mut parsed = Vec::with_capacity(parsed_results.len());
    for result in parsed_results {
        match result {
            Ok(file) => parsed.push(file),
            Err(e) => {
                stats.files_failed += 1;
                tracing::warn!(error = %e, "skipping file during structural index");
            }
        }
    }

    if verbose {
        tracing::info!(
            parsed = parsed.len(),
            failed = stats.files_failed,
            "structural parse complete, writing to db"
        );
    }

    db.execute_batch(|db| {
        for p in &parsed {
            let file_id = db.upsert_file(&p.info)?;
            db.insert_symbols(file_id, &p.symbols)?;
            db.insert_references(file_id, &p.refs)?;
            db.insert_fingerprints(file_id, &fingerprint_inputs(p))?;
            // Trust-boundary derivation runs against the in-memory refs we
            // just inserted — no DB round-trip to re-read them. Replace
            // whatever was stored previously so re-index keeps the set
            // current as imports change.
            let boundaries = derive_from_refs(&p.refs, p.info.language);
            db.replace_file_trust_boundaries(file_id, &boundaries)?;
        }
        Ok(())
    })?;
    for p in &parsed {
        stats.symbols_found += p.symbols.len();
        stats.references_found += p.refs.len();
        stats.files_indexed += 1;
    }

    Ok(stats)
}

pub fn full_index(
    root: &Path,
    db: &Database,
    exclude_patterns: &[String],
    verbose: bool,
) -> Result<IndexStats> {
    index(root, db, exclude_patterns, IndexStrategy::Full, verbose)
}

pub fn incremental_index(
    root: &Path,
    db: &Database,
    exclude_patterns: &[String],
    verbose: bool,
) -> Result<IndexStats> {
    index(
        root,
        db,
        exclude_patterns,
        IndexStrategy::Incremental,
        verbose,
    )
}

pub fn index_files(
    root: &Path,
    db: &Database,
    files: &[FileInfo],
    verbose: bool,
) -> Result<IndexStats> {
    let mut stats = IndexStats::default();

    if files.is_empty() {
        return Ok(stats);
    }

    if verbose {
        tracing::info!(count = files.len(), "indexing specific files");
    }

    let parsed_results: Vec<Result<ParsedFile>> =
        files.par_iter().map(|f| parse_one(root, f)).collect();
    let mut parsed = Vec::with_capacity(parsed_results.len());
    for result in parsed_results {
        match result {
            Ok(file) => parsed.push(file),
            Err(e) => {
                stats.files_failed += 1;
                tracing::warn!(error = %e, "skipping file during per-file structural index");
            }
        }
    }

    db.execute_batch(|db| {
        for p in &parsed {
            let file_id = db.upsert_file(&p.info)?;
            db.insert_symbols(file_id, &p.symbols)?;
            db.insert_references(file_id, &p.refs)?;
            db.insert_fingerprints(file_id, &fingerprint_inputs(p))?;
            let boundaries = derive_from_refs(&p.refs, p.info.language);
            db.replace_file_trust_boundaries(file_id, &boundaries)?;
        }
        Ok(())
    })?;
    for p in &parsed {
        stats.symbols_found += p.symbols.len();
        stats.references_found += p.refs.len();
        stats.files_indexed += 1;
    }

    Ok(stats)
}

pub fn remove_files(db: &Database, paths: &[String]) -> Result<usize> {
    let mut removed = 0;
    db.execute_batch(|db| {
        for path in paths {
            db.remove_file(path)?;
            removed += 1;
        }
        Ok(())
    })?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codesage_protocol::Language;

    #[test]
    fn parse_one_reports_missing_file() {
        let root = tempfile::tempdir().unwrap();
        let file = FileInfo {
            path: "missing.rs".to_string(),
            language: Language::Rust,
            content_hash: "h".to_string(),
        };

        let err = parse_one(root.path(), &file).unwrap_err();

        assert!(
            err.to_string().contains("reading missing.rs"),
            "unexpected error: {err:#}"
        );
    }
}
