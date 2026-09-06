use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use codesage_features::trust_boundary::derive_from_refs;
use codesage_protocol::{FileInfo, IndexStats, Reference, Symbol};
use codesage_storage::db::FingerprintInput;
use codesage_storage::{Database, is_unique_violation};
use rayon::prelude::*;

use codesage_parser::discover::discover_files_with_excludes;
use codesage_parser::extract::extract_symbols;
use codesage_parser::fingerprint::{FunctionFingerprint, file_fingerprints};
use codesage_parser::parse::{ParsedTree, parse_file_tolerant};
use codesage_parser::references::extract_references;

#[derive(Debug)]
struct ParsedFile {
    info: FileInfo,
    symbols: Vec<Symbol>,
    refs: Vec<Reference>,
    fingerprints: Vec<FunctionFingerprint>,
    /// The tree carried `ERROR` / `MISSING` nodes; everything outside them
    /// was still extracted.
    degraded: bool,
}

fn parse_one(root: &Path, file_info: &FileInfo) -> Result<ParsedFile> {
    let abs_path = root.join(&file_info.path);
    let source = std::fs::read(&abs_path).with_context(|| format!("reading {}", file_info.path))?;
    let ParsedTree { tree, degraded } = parse_file_tolerant(&source, file_info.language)
        .with_context(|| format!("parsing {}", file_info.path))?;
    let symbols = extract_symbols(&tree, &source, file_info.language, &file_info.path)
        .with_context(|| format!("extracting symbols from {}", file_info.path))?;
    let mut refs = extract_references(&tree, &source, file_info.language, &file_info.path)
        .with_context(|| format!("extracting references from {}", file_info.path))?;
    dedupe_refs(&mut refs);
    populate_from_symbol(&symbols, &mut refs);
    let fingerprints = file_fingerprints(&tree, &source, file_info.language);
    Ok(ParsedFile {
        info: file_info.clone(),
        symbols,
        refs,
        fingerprints,
        degraded,
    })
}

/// Collapse references sharing the `refs` UNIQUE key `(to_name, kind, line,
/// col)` within one file. Two query patterns matching one node would
/// otherwise abort the whole write batch on the index; `references_found`
/// then counts stored rows.
fn dedupe_refs(refs: &mut Vec<Reference>) {
    let mut seen = HashSet::new();
    refs.retain(|r| seen.insert((r.to_name.clone(), r.kind, r.line, r.col)));
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

const STRUCTURAL_INDEX_BATCH_SIZE: usize = 50;

/// Parse one batch in parallel. A file that fails (unreadable, or the parser
/// returned no tree) is logged with its path and full cause chain, recorded
/// in `stats.files_failed` / `stats.failed_paths`, and dropped from the
/// returned set; the rest of the batch proceeds. A degraded parse is not a
/// failure: it is logged at debug and counted when the batch is written.
fn parse_batch(root: &Path, batch: &[&FileInfo], stats: &mut IndexStats) -> Vec<ParsedFile> {
    let results: Vec<(&FileInfo, Result<ParsedFile>)> =
        batch.par_iter().map(|f| (*f, parse_one(root, f))).collect();
    let mut parsed = Vec::with_capacity(results.len());
    for (file, result) in results {
        match result {
            Ok(p) => {
                if p.degraded {
                    tracing::debug!(
                        path = %file.path,
                        "structural parse recovered from syntax errors; symbols outside the damaged regions were extracted"
                    );
                }
                parsed.push(p);
            }
            Err(e) => {
                stats.files_failed += 1;
                stats.failed_paths.push(file.path.clone());
                tracing::warn!(
                    path = %file.path,
                    error = %format!("{e:#}"),
                    "skipping file during structural index"
                );
            }
        }
    }
    parsed
}

/// Write one file's rows inside the caller's transaction.
fn write_one(db: &Database, p: &ParsedFile) -> Result<()> {
    let file_id = db.upsert_file(&p.info)?;
    db.insert_symbols(file_id, &p.symbols)?;
    db.insert_references(file_id, &p.refs)?;
    db.insert_fingerprints(file_id, &fingerprint_inputs(p))?;
    let boundaries = derive_from_refs(&p.refs, p.info.language);
    db.replace_file_trust_boundaries(file_id, &boundaries)?;
    Ok(())
}

fn count_written(stats: &mut IndexStats, p: &ParsedFile) {
    stats.symbols_found += p.symbols.len();
    stats.references_found += p.refs.len();
    stats.files_indexed += 1;
    if p.degraded {
        stats.files_degraded += 1;
    }
}

/// One transaction per batch: a failing statement rolls the whole batch
/// back, so the tables never hold a half-written file, while earlier batches
/// stay committed. Extraction collapses rows on the `symbols` / `refs` /
/// `symbol_fingerprints` UNIQUE keys, so the batch path is the normal one.
/// When it does fail, the batch is retried one file per transaction. Only a
/// UNIQUE / PRIMARY KEY violation is the file's fault: that file is logged
/// with path and cause, counted in `stats.files_failed` /
/// `stats.failed_paths`, and isolated while the rest commit. Every other
/// database error (full or read-only database, I/O error, lock held past
/// `busy_timeout`, damaged schema, a CHECK / NOT NULL / FOREIGN KEY failure
/// that would reject every file alike) aborts the pass with `Err`, leaving
/// existing rows in place: a full pass purges the rows of every failed path,
/// so treating such a fault as N file failures would delete committed data.
fn write_parsed_batch(db: &Database, parsed: &[ParsedFile], stats: &mut IndexStats) -> Result<()> {
    if parsed.is_empty() {
        return Ok(());
    }

    let batch_result = db.execute_batch(|db| {
        for p in parsed {
            write_one(db, p)?;
        }
        Ok(())
    });
    match batch_result {
        Ok(()) => {
            for p in parsed {
                count_written(stats, p);
            }
            return Ok(());
        }
        Err(e) => tracing::warn!(
            files = parsed.len(),
            error = %format!("{e:#}"),
            "structural write batch failed; retrying each file in its own transaction"
        ),
    }

    for p in parsed {
        match db.execute_batch(|db| write_one(db, p)) {
            Ok(()) => count_written(stats, p),
            Err(e) if !is_unique_violation(&e) => {
                return Err(e).with_context(|| {
                    format!(
                        "writing structural rows for {}: database error, aborting the pass",
                        p.info.path
                    )
                });
            }
            Err(e) => {
                stats.files_failed += 1;
                stats.failed_paths.push(p.info.path.clone());
                tracing::warn!(
                    path = %p.info.path,
                    error = %format!("{e:#}"),
                    "skipping file during structural index: write rejected"
                );
            }
        }
    }
    Ok(())
}

fn index(
    root: &Path,
    db: &Database,
    exclude_patterns: &[String],
    strategy: IndexStrategy,
    verbose: bool,
) -> Result<IndexStats> {
    let files = discover_files_with_excludes(root, exclude_patterns)?;
    index_discovered(root, db, &files, strategy, verbose)
}

/// Index an already-discovered file set. `files` is the complete set for
/// `root`: paths in the table but not in `files` are removed as orphans.
fn index_discovered(
    root: &Path,
    db: &Database,
    files: &[FileInfo],
    strategy: IndexStrategy,
    verbose: bool,
) -> Result<IndexStats> {
    let mut stats = IndexStats::default();

    if verbose {
        tracing::info!(total = files.len(), "discovered files for structural index");
    }

    let discovered_paths: HashSet<&str> = files.iter().map(|f| f.path.as_str()).collect();
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

    for batch in to_parse.chunks(STRUCTURAL_INDEX_BATCH_SIZE) {
        let parsed = parse_batch(root, batch, &mut stats);

        if verbose {
            tracing::info!(
                parsed = parsed.len(),
                failed = stats.files_failed,
                "structural parse batch complete, writing to db"
            );
        }
        write_parsed_batch(db, &parsed, &mut stats)?;
    }

    // A full pass rewrites the whole table, so a file it could not process
    // must not keep rows from an earlier pass: they would describe a version
    // of the file nobody can see. Dropping the `files` row also drops the
    // stored hash, so the next incremental pass retries the file instead of
    // reading the absence as "unchanged". An incremental pass keeps the old
    // rows: it never saw the whole table and a transient read failure (an
    // editor mid-write) should not blank a file's symbols.
    if strategy == IndexStrategy::Full && !stats.failed_paths.is_empty() {
        db.execute_batch(|db| {
            for path in &stats.failed_paths {
                db.remove_structural_file(path)?;
            }
            Ok(())
        })?;
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

/// Index the named files only. Never purges a failed file's rows: this pass
/// does not see the whole table.
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

    let file_refs: Vec<&FileInfo> = files.iter().collect();
    for batch in file_refs.chunks(STRUCTURAL_INDEX_BATCH_SIZE) {
        let parsed = parse_batch(root, batch, &mut stats);
        write_parsed_batch(db, &parsed, &mut stats)?;
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
    use codesage_parser::discover::content_hash;
    use codesage_protocol::{Language, ReferenceKind, SymbolKind};

    fn file(root: &Path, path: &str, language: Language, source: &[u8]) -> FileInfo {
        std::fs::write(root.join(path), source).unwrap();
        FileInfo {
            path: path.to_string(),
            language,
            content_hash: content_hash(source),
        }
    }

    /// A file discovery listed but the pass cannot read (deleted or made
    /// unreadable between the two, or any other per-file read error).
    fn unreadable(path: &str, language: Language) -> FileInfo {
        FileInfo {
            path: path.to_string(),
            language,
            content_hash: "gone".to_string(),
        }
    }

    fn seed_symbol(db: &Database, info: &FileInfo, name: &str) {
        let file_id = db.upsert_file(info).unwrap();
        db.insert_symbols(
            file_id,
            &[Symbol {
                name: name.to_string(),
                qualified_name: name.to_string(),
                kind: SymbolKind::Function,
                file_path: info.path.clone(),
                line_start: 1,
                line_end: 1,
                col_start: 0,
                col_end: 1,
                rationale: Vec::new(),
            }],
        )
        .unwrap();
    }

    const UNKNOWN_MACRO_C: &[u8] = b"ZEND_BEGIN_ARG_INFO_EX(arginfo_add, 0, 0, 2)\n\
        \tZEND_ARG_INFO(0, a)\n\
        ZEND_END_ARG_INFO()\n\
        \n\
        int add(int a, int b) {\n\
        \treturn a + b;\n\
        }\n";

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

    #[test]
    fn dedupe_refs_collapses_rows_sharing_the_unique_key() {
        let r = |to_name: &str, kind, line, col| Reference {
            from_file: "a.c".to_string(),
            from_symbol: None,
            to_name: to_name.to_string(),
            kind,
            line,
            col,
        };
        let mut refs = vec![
            r("foo", ReferenceKind::Call, 3, 4),
            r("foo", ReferenceKind::Call, 3, 4),
            r("foo", ReferenceKind::Call, 3, 9),
            r("foo", ReferenceKind::Import, 3, 4),
        ];

        dedupe_refs(&mut refs);

        assert_eq!(refs.len(), 3, "{refs:?}");
        assert_eq!(refs[0].col, 4);
        assert_eq!(refs[1].col, 9);
        assert_eq!(refs[2].kind, ReferenceKind::Import);
    }

    fn parsed(root: &Path, path: &str, language: Language, source: &[u8]) -> ParsedFile {
        parse_one(root, &file(root, path, language, source)).unwrap()
    }

    #[test]
    fn write_fallback_fails_the_offending_file_and_commits_the_rest() {
        let root = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let clean = parsed(
            root.path(),
            "a.c",
            Language::C,
            b"int a(void) { return 1; }\n",
        );
        // Bypass extraction dedupe: two symbol rows sharing the
        // `uq_symbols_identity` key.
        let mut bad = parsed(
            root.path(),
            "b.c",
            Language::C,
            b"int b(void) { return 2; }\n",
        );
        assert_eq!(bad.symbols.len(), 1);
        let twin = bad.symbols[0].clone();
        bad.symbols.push(twin);
        let batch = vec![clean, bad];
        let mut stats = IndexStats::default();

        write_parsed_batch(&db, &batch, &mut stats).unwrap();

        assert_eq!(stats.files_indexed, 1, "{stats:?}");
        assert_eq!(stats.files_failed, 1, "{stats:?}");
        assert_eq!(stats.failed_paths, vec!["b.c".to_string()], "{stats:?}");
        assert_eq!(stats.symbols_found, 1, "{stats:?}");
        assert_eq!(db.symbols_for_file("a.c").unwrap().len(), 1);
        assert!(
            db.symbols_for_file("b.c").unwrap().is_empty(),
            "the rejected file's transaction must roll back whole"
        );
        assert!(
            !db.all_file_hashes().unwrap().contains_key("b.c"),
            "no `files` row either, so the next pass retries b.c"
        );
    }

    #[test]
    fn write_fallback_covers_duplicate_fingerprint_rows() {
        let root = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let clean = parsed(
            root.path(),
            "a.c",
            Language::C,
            b"int a(void) { return 1; }\n",
        );
        let mut bad = parsed(
            root.path(),
            "b.c",
            Language::C,
            b"int dup(int a,int b){int s=0;for(int i=0;i<a;i++){if(i%2){s+=i*b;}else{s-=1;}}return s;}\n",
        );
        assert_eq!(bad.fingerprints.len(), 1, "{bad:?}");
        let twin = bad.fingerprints[0].clone();
        bad.fingerprints.push(twin);
        let batch = vec![bad, clean];
        let mut stats = IndexStats::default();

        write_parsed_batch(&db, &batch, &mut stats).unwrap();

        assert_eq!(stats.files_indexed, 1, "{stats:?}");
        assert_eq!(stats.failed_paths, vec!["b.c".to_string()], "{stats:?}");
        assert_eq!(db.symbols_for_file("a.c").unwrap().len(), 1);
        assert_eq!(db.all_fingerprints().unwrap().len(), 0);
    }

    /// A file-backed index opened twice: `writer` seeds and later inspects
    /// rows, `read_only` makes every write fail with `SQLITE_READONLY`, a
    /// database-level fault that is nobody file's fault.
    fn read_only_pair(dir: &Path) -> (Database, Database) {
        let path = dir.join("index.db");
        let writer = Database::open(&path).unwrap();
        let read_only = Database::open_read_only(&path).unwrap();
        (writer, read_only)
    }

    #[test]
    fn non_constraint_write_error_aborts_the_batch_without_failing_files() {
        let root = tempfile::tempdir().unwrap();
        let (_writer, read_only) = read_only_pair(root.path());
        let batch = vec![
            parsed(
                root.path(),
                "a.c",
                Language::C,
                b"int a(void) { return 1; }\n",
            ),
            parsed(
                root.path(),
                "b.c",
                Language::C,
                b"int b(void) { return 2; }\n",
            ),
        ];
        let mut stats = IndexStats::default();

        let err = write_parsed_batch(&read_only, &batch, &mut stats).unwrap_err();

        assert!(
            !is_unique_violation(&err),
            "the injected fault must not read as a unique violation: {err:#}"
        );
        assert_eq!(stats.files_failed, 0, "{stats:?}");
        assert!(stats.failed_paths.is_empty(), "{stats:?}");
        assert_eq!(stats.files_indexed, 0, "{stats:?}");
    }

    /// The fault is a renamed `file_trust_boundaries` table on a writable
    /// handle: `upsert_file`'s `DELETE FROM file_trust_boundaries` fails
    /// with SQLITE_ERROR ("no such table"), a non-constraint fault, while
    /// the purge's `DELETE FROM files` still works. A read-only handle would
    /// not do: there the purge fails too, so the test would pass even if the
    /// fault were misread as N per-file failures.
    #[test]
    fn full_pass_aborts_on_database_fault_and_keeps_existing_rows() {
        let root = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let stale = file(
            root.path(),
            "b.c",
            Language::C,
            b"int b(void) { return 2; }\n",
        );
        seed_symbol(&db, &stale, "old_b");
        let files = vec![
            file(
                root.path(),
                "a.c",
                Language::C,
                b"int a(void) { return 1; }\n",
            ),
            stale,
        ];
        db.execute_raw_for_tests(
            "ALTER TABLE file_trust_boundaries RENAME TO file_trust_boundaries_off",
        )
        .unwrap();

        let err =
            index_discovered(root.path(), &db, &files, IndexStrategy::Full, false).unwrap_err();

        assert!(!is_unique_violation(&err), "{err:#}");
        assert_eq!(
            db.symbols_for_file("b.c").unwrap().len(),
            1,
            "a database fault must abort the pass before the purge, not delete committed rows"
        );
        assert!(
            db.all_file_hashes().unwrap().contains_key("b.c"),
            "the stored hash survives too"
        );
        assert!(db.symbols_for_file("a.c").unwrap().is_empty());
    }

    #[test]
    fn same_named_definitions_on_one_line_index_as_one_fingerprint() {
        let root = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let files = vec![
            file(
                root.path(),
                "bundle.js",
                Language::JavaScript,
                b"function n(a,b){var s=0;for(var i=0;i<a;i++){if(i%2){s+=i*b}else{s-=1}}return s} function n(a,b){var s=0;for(var i=0;i<a;i++){if(i%2){s+=i*b}else{s-=1}}return s}\n",
            ),
            file(
                root.path(),
                "gen.c",
                Language::C,
                b"int dup(int a,int b){int s=0;for(int i=0;i<a;i++){if(i%2){s+=i*b;}else{s-=1;}}return s;} int dup(int a,int b){int s=0;for(int i=0;i<a;i++){if(i%2){s+=i*b;}else{s-=1;}}return s;}\n",
            ),
        ];

        let stats = index_discovered(root.path(), &db, &files, IndexStrategy::Full, false).unwrap();

        assert_eq!(stats.files_failed, 0, "{stats:?}");
        assert_eq!(stats.files_indexed, 2, "{stats:?}");
        let fps = db.all_fingerprints().unwrap();
        assert_eq!(fps.len(), 2, "one row per file: {fps:?}");
    }

    #[test]
    fn degraded_parse_is_indexed_and_counted_not_failed() {
        let root = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let files = vec![file(root.path(), "ext.c", Language::C, UNKNOWN_MACRO_C)];

        let stats = index_discovered(root.path(), &db, &files, IndexStrategy::Full, false).unwrap();

        assert_eq!(stats.files_failed, 0, "{stats:?}");
        assert!(stats.failed_paths.is_empty(), "{stats:?}");
        assert_eq!(stats.files_indexed, 1, "{stats:?}");
        assert_eq!(stats.files_degraded, 1, "{stats:?}");
        let names: Vec<String> = db
            .symbols_for_file("ext.c")
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, vec!["add".to_string()]);
    }

    #[test]
    fn clean_parse_is_not_counted_as_degraded() {
        let root = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let files = vec![file(
            root.path(),
            "a.c",
            Language::C,
            b"int add(int a, int b) { return a + b; }\n",
        )];

        let stats = index_discovered(root.path(), &db, &files, IndexStrategy::Full, false).unwrap();

        assert_eq!(stats.files_degraded, 0, "{stats:?}");
        assert_eq!(stats.files_indexed, 1, "{stats:?}");
    }

    #[test]
    fn failed_file_is_named_and_the_rest_of_the_batch_is_written() {
        let root = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let files = vec![
            file(
                root.path(),
                "a.c",
                Language::C,
                b"int a(void) { return 1; }\n",
            ),
            unreadable("b.c", Language::C),
            file(
                root.path(),
                "c.c",
                Language::C,
                b"int c(void) { return 3; }\n",
            ),
        ];

        let stats = index_discovered(root.path(), &db, &files, IndexStrategy::Full, false).unwrap();

        assert_eq!(stats.files_failed, 1, "{stats:?}");
        assert_eq!(stats.failed_paths, vec!["b.c".to_string()], "{stats:?}");
        assert_eq!(stats.files_indexed, 2, "{stats:?}");
        assert_eq!(db.symbols_for_file("a.c").unwrap().len(), 1);
        assert_eq!(db.symbols_for_file("c.c").unwrap().len(), 1);
    }

    #[test]
    fn full_pass_purges_structural_rows_of_a_file_it_cannot_read() {
        let root = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let stale = unreadable("b.c", Language::C);
        seed_symbol(&db, &stale, "old_b");
        let files = vec![
            file(
                root.path(),
                "a.c",
                Language::C,
                b"int a(void) { return 1; }\n",
            ),
            stale,
        ];

        let stats = index_discovered(root.path(), &db, &files, IndexStrategy::Full, false).unwrap();

        assert_eq!(stats.failed_paths, vec!["b.c".to_string()], "{stats:?}");
        assert!(
            db.symbols_for_file("b.c").unwrap().is_empty(),
            "rows this pass could not rewrite must not survive a full pass"
        );
        assert!(
            !db.all_file_hashes().unwrap().contains_key("b.c"),
            "the stored hash must go too, so the next incremental pass retries b.c"
        );
        assert_eq!(db.symbols_for_file("a.c").unwrap().len(), 1);
    }

    #[test]
    fn incremental_pass_keeps_structural_rows_of_a_file_it_cannot_read() {
        let root = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let stale = unreadable("b.c", Language::C);
        seed_symbol(&db, &stale, "old_b");
        // A different hash than the seeded row, so the pass selects b.c.
        let files = vec![FileInfo {
            content_hash: "changed".to_string(),
            ..stale
        }];

        let stats =
            index_discovered(root.path(), &db, &files, IndexStrategy::Incremental, false).unwrap();

        assert_eq!(stats.failed_paths, vec!["b.c".to_string()], "{stats:?}");
        assert_eq!(
            db.symbols_for_file("b.c").unwrap().len(),
            1,
            "an incremental pass never saw the whole table and keeps the old rows"
        );
    }
}
