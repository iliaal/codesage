//! Files / symbols / refs / dependencies.

use anyhow::Result;
use codesage_protocol::{
    DependencyEntry, FileInfo, Language, RationaleEntry, Reference, ReferenceKind, Symbol,
    SymbolKind, TrustBoundary,
};
use rusqlite::params;

use crate::schema::name_tail;

use super::{
    Database, get_index_state, row_enum, row_reference_kind, row_symbol_kind, set_index_state,
};

/// Decode the JSON-encoded `rationale` column. A malformed value (manual DB
/// edit, schema drift, etc.) becomes an empty Vec rather than failing the
/// row read — rationale is auxiliary metadata and a corrupt entry must not
/// break `find_symbol`.
fn deserialize_rationale(s: &str) -> Vec<RationaleEntry> {
    serde_json::from_str(s).unwrap_or_default()
}

/// Map a `(name, qualified_name, kind, path, line_start, line_end, col_start,
/// col_end, rationale)` row — the column order shared by `find_symbols`,
/// `symbols_for_files`, and `symbols_for_file` — into a `Symbol`.
fn row_to_symbol(row: &rusqlite::Row<'_>) -> rusqlite::Result<Symbol> {
    let kind_str: String = row.get(2)?;
    let rationale_json: String = row.get(8)?;
    Ok(Symbol {
        name: row.get(0)?,
        qualified_name: row.get(1)?,
        kind: row_symbol_kind(&kind_str)?,
        file_path: row.get(3)?,
        line_start: row.get(4)?,
        line_end: row.get(5)?,
        col_start: row.get(6)?,
        col_end: row.get(7)?,
        rationale: deserialize_rationale(&rationale_json),
    })
}

/// Map an `(id, path, language)` row into its typed triple. Shared by
/// `all_files_with_id_and_language` and `files_pending_boundary_derivation`,
/// which differ only in their WHERE clause.
fn row_to_file_lang(row: &rusqlite::Row<'_>) -> rusqlite::Result<(i64, String, Language)> {
    let id: i64 = row.get(0)?;
    let path: String = row.get(1)?;
    let lang_str: String = row.get(2)?;
    Ok((id, path, row_enum(&lang_str, Language::parse, "Language")?))
}

/// Borrowed fingerprint row for insertion. `fp` is the MinHash signature.
pub struct FingerprintInput<'a> {
    pub name: &'a str,
    pub kind: &'a str,
    pub line_start: u32,
    pub line_end: u32,
    pub leaf_count: u32,
    pub fp: &'a [u64],
}

/// Owned fingerprint row as read back, with its file path and language.
#[derive(Debug, Clone)]
pub struct StoredFingerprint {
    pub file_path: String,
    pub language: String,
    pub name: String,
    pub kind: String,
    pub line_start: u32,
    pub line_end: u32,
    pub leaf_count: u32,
    pub fp: Vec<u64>,
}

fn fp_to_blob(fp: &[u64]) -> Vec<u8> {
    let mut b = Vec::with_capacity(fp.len() * 8);
    for &x in fp {
        b.extend_from_slice(&x.to_le_bytes());
    }
    b
}

fn blob_to_fp(b: &[u8]) -> Vec<u64> {
    // A non-8-multiple blob is corrupt; return empty so the caller's
    // fixed-length conversion rejects it rather than silently dropping the
    // trailing bytes and decoding a plausible-but-wrong signature.
    if !b.len().is_multiple_of(8) {
        return Vec::new();
    }
    b.as_chunks::<8>()
        .0
        .iter()
        .map(|c| u64::from_le_bytes(*c))
        .collect()
}

fn stored_fingerprint_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredFingerprint> {
    let blob: Vec<u8> = row.get(7)?;
    Ok(StoredFingerprint {
        file_path: row.get(0)?,
        language: row.get(1)?,
        name: row.get(2)?,
        kind: row.get(3)?,
        line_start: row.get(4)?,
        line_end: row.get(5)?,
        leaf_count: row.get(6)?,
        fp: blob_to_fp(&blob),
    })
}

impl Database {
    /// Return `(last_sha, last_indexed_at_unix)` for the structural index if a
    /// stamp exists. Mirrors [`Database::get_git_index_state`] but tracks the
    /// structural/semantic layer — not the git history layer. Used by drift
    /// instrumentation (see `codesage doctor`) to detect cases where git hooks
    /// failed to trigger a reindex.
    pub fn get_structural_index_state(&self) -> Result<Option<(String, i64)>> {
        get_index_state(&self.conn, "structural_index_state")
    }

    /// Stamp the HEAD SHA that the structural index was just built against.
    /// `indexed_at` is set to `unixepoch()` at the DB. Callers must only pass
    /// real SHAs — the "not a git repo" case is the caller's to skip.
    pub fn set_structural_index_state(&self, sha: &str) -> Result<()> {
        set_index_state(&self.conn, "structural_index_state", sha)
    }

    // prepare_cached throughout the per-file index loop (`upsert_file` +
    // `insert_symbols` / `insert_references` / `insert_fingerprints`): these
    // run once per file, so a plain `prepare` re-plans the same SQL thousands
    // of times on a large repo. Connections are long-lived per index pass, so
    // the cached statements are reused across every file.
    pub fn upsert_file(&self, file: &FileInfo) -> Result<i64> {
        self.conn
            .prepare_cached(
                "INSERT INTO files (path, language, content_hash, indexed_at)
                 VALUES (?1, ?2, ?3, unixepoch())
                 ON CONFLICT(path) DO UPDATE SET
                   language = excluded.language,
                   content_hash = excluded.content_hash,
                   indexed_at = excluded.indexed_at",
            )?
            .execute(params![
                file.path,
                file.language.as_str(),
                file.content_hash
            ])?;

        let file_id: i64 = self
            .conn
            .prepare_cached("SELECT id FROM files WHERE path = ?1")?
            .query_row(params![file.path], |row| row.get(0))?;

        self.conn
            .prepare_cached("DELETE FROM symbols WHERE file_id = ?1")?
            .execute(params![file_id])?;
        self.conn
            .prepare_cached("DELETE FROM refs WHERE from_file_id = ?1")?
            .execute(params![file_id])?;
        self.conn
            .prepare_cached("DELETE FROM symbol_fingerprints WHERE file_id = ?1")?
            .execute(params![file_id])?;
        self.conn
            .prepare_cached("DELETE FROM file_trust_boundaries WHERE file_id = ?1")?
            .execute(params![file_id])?;
        self.conn
            .prepare_cached("UPDATE files SET boundaries_derived_at = 0 WHERE id = ?1")?
            .execute(params![file_id])?;

        Ok(file_id)
    }

    pub fn insert_symbols(&self, file_id: i64, symbols: &[Symbol]) -> Result<()> {
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO symbols (file_id, name, qualified_name, kind, line_start, line_end, col_start, col_end, rationale)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;

        for s in symbols {
            // Empty rationale serializes to "[]"; non-empty serializes the
            // full Vec. Failure here would mean a serde bug, not a data
            // problem — fall back to the empty marker rather than aborting
            // the whole insert pass.
            let rationale_json =
                serde_json::to_string(&s.rationale).unwrap_or_else(|_| "[]".to_string());
            stmt.execute(params![
                file_id,
                s.name,
                s.qualified_name,
                s.kind.as_str(),
                s.line_start,
                s.line_end,
                s.col_start,
                s.col_end,
                rationale_json,
            ])?;
        }
        Ok(())
    }

    pub fn insert_references(&self, file_id: i64, refs: &[Reference]) -> Result<()> {
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO refs (from_file_id, from_symbol, to_name, to_name_tail, kind, line, col)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;

        for r in refs {
            stmt.execute(params![
                file_id,
                r.from_symbol,
                r.to_name,
                name_tail(&r.to_name),
                r.kind.as_str(),
                r.line,
                r.col,
            ])?;
        }
        Ok(())
    }

    /// Persist MinHash fingerprints for one file's functions/methods. Caller
    /// deletes prior rows via `upsert_file`; this only inserts.
    pub fn insert_fingerprints(&self, file_id: i64, fps: &[FingerprintInput<'_>]) -> Result<()> {
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO symbol_fingerprints
                 (file_id, name, kind, line_start, line_end, leaf_count, fp)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for f in fps {
            stmt.execute(params![
                file_id,
                f.name,
                f.kind,
                f.line_start,
                f.line_end,
                f.leaf_count,
                fp_to_blob(f.fp),
            ])?;
        }
        Ok(())
    }

    /// Every stored function fingerprint with its file path. Loaded whole by
    /// `find_similar`, which builds the LSH index in memory. Scales with the
    /// function count; fine for repos up to the low hundreds of thousands.
    pub fn all_fingerprints(&self) -> Result<Vec<StoredFingerprint>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.path, f.language, sf.name, sf.kind, sf.line_start, sf.line_end, sf.leaf_count, sf.fp
             FROM symbol_fingerprints sf JOIN files f ON sf.file_id = f.id",
        )?;
        let rows = stmt.query_map([], stored_fingerprint_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn fingerprints_named(&self, name: &str) -> Result<Vec<StoredFingerprint>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.path, f.language, sf.name, sf.kind, sf.line_start, sf.line_end, sf.leaf_count, sf.fp
             FROM symbol_fingerprints sf JOIN files f ON sf.file_id = f.id
             WHERE sf.name = ?1",
        )?;
        let rows = stmt.query_map(params![name], stored_fingerprint_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn fingerprints_for_language(&self, language: &str) -> Result<Vec<StoredFingerprint>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.path, f.language, sf.name, sf.kind, sf.line_start, sf.line_end, sf.leaf_count, sf.fp
             FROM symbol_fingerprints sf JOIN files f ON sf.file_id = f.id
             WHERE f.language = ?1",
        )?;
        let rows = stmt.query_map(params![language], stored_fingerprint_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn fingerprint_cache_key(&self) -> Option<String> {
        self.conn
            .path()
            .filter(|p| !p.is_empty())
            .map(str::to_string)
    }

    pub fn fingerprint_validity_token(&self) -> Result<(i64, i64, i64, i64)> {
        let token = self.conn.query_row(
            "SELECT COUNT(*),
                    COALESCE(MAX(sf.rowid), 0),
                    COALESCE(SUM(length(f.path)), 0),
                    COALESCE(SUM(sf.line_start + sf.line_end + sf.leaf_count), 0)
             FROM symbol_fingerprints sf JOIN files f ON sf.file_id = f.id",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        Ok(token)
    }

    /// Resolve a repo-relative path to its `files.id`, or `None` if the
    /// file isn't indexed. Used by the feature mapper to attach
    /// framework-derived references (route edges) to their source file.
    pub fn file_id_for_path(&self, path: &str) -> Result<Option<i64>> {
        let mut stmt = self.conn.prepare("SELECT id FROM files WHERE path = ?1")?;
        match stmt.query_row(params![path], |row| row.get::<_, i64>(0)) {
            Ok(id) => Ok(Some(id)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn indexed_files_with_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        let pattern = format!("{}%", escape_like(prefix));
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM files WHERE path LIKE ?1 ESCAPE '\\' ORDER BY path")?;
        let rows = stmt
            .query_map(params![pattern], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Delete every reference of a given kind across all files. The feature
    /// mapper calls this before re-inserting synthetic edges (e.g.
    /// `RouteHandler`) so a remap stays idempotent and drops edges whose
    /// route declaration was removed.
    pub fn delete_references_of_kind(&self, kind: ReferenceKind) -> Result<usize> {
        let n = self
            .conn
            .execute("DELETE FROM refs WHERE kind = ?1", params![kind.as_str()])?;
        Ok(n)
    }

    pub fn get_file_hash(&self, path: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT content_hash FROM files WHERE path = ?1")?;
        let result = stmt.query_row(params![path], |row| row.get(0));
        match result {
            Ok(hash) => Ok(Some(hash)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Single-query preload of every (path, content_hash) row from the `files`
    /// table. Callers use this instead of `get_file_hash` in a loop to avoid an
    /// N+1 on large repos (25k+ files × one round-trip each is dominant vs one
    /// sequential scan that returns everything).
    pub fn all_file_hashes(&self) -> Result<std::collections::HashMap<String, String>> {
        let mut stmt = self.conn.prepare("SELECT path, content_hash FROM files")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<std::collections::HashMap<_, _>>>()?;
        Ok(rows)
    }

    /// Cheap existence test for a symbol name. Used by the search-boost pipeline
    /// which only cares whether the token matches any indexed symbol, not the
    /// full row contents. Matches `find_symbols`' branch shape (qualified name
    /// goes against `qualified_name`, bare goes against `name`) and uses exact
    /// match in both — the boost heuristic over-triggers on substrings.
    pub fn symbol_exists(&self, name: &str) -> Result<bool> {
        let sql = if name.contains('\\') || name.contains('.') || name.contains("::") {
            "SELECT 1 FROM symbols WHERE qualified_name = ?1 LIMIT 1"
        } else {
            "SELECT 1 FROM symbols WHERE name = ?1 LIMIT 1"
        };
        // prepare_cached: symbol_exists is called once per query token, and
        // find_symbols/find_references run in per-reference loops during bundle
        // assembly. Connections are per-tool-call, so the cached statement is
        // reused across the loop iterations within one call. Only two distinct
        // SQL strings here, so the cache stays tiny.
        let mut stmt = self.conn.prepare_cached(sql)?;
        match stmt.query_row(params![name], |_| Ok(())) {
            Ok(()) => Ok(true),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    pub fn find_symbols(&self, name: &str, kind: Option<SymbolKind>) -> Result<Vec<Symbol>> {
        let sql = if name.contains('\\') || name.contains('.') || name.contains("::") {
            "SELECT s.name, s.qualified_name, s.kind, f.path, s.line_start, s.line_end, s.col_start, s.col_end, s.rationale
              FROM symbols s JOIN files f ON s.file_id = f.id
              WHERE s.qualified_name = ?1"
        } else {
            "SELECT s.name, s.qualified_name, s.kind, f.path, s.line_start, s.line_end, s.col_start, s.col_end, s.rationale
              FROM symbols s JOIN files f ON s.file_id = f.id
              WHERE s.name = ?1"
        };

        let mut stmt = self.conn.prepare_cached(sql)?;
        let rows = stmt.query_map(params![name], row_to_symbol)?;

        let mut symbols: Vec<Symbol> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        if let Some(k) = kind {
            symbols.retain(|s| s.kind == k);
        }
        Ok(symbols)
    }

    pub fn find_references(
        &self,
        to_name: &str,
        kind: Option<ReferenceKind>,
    ) -> Result<Vec<Reference>> {
        let is_qualified =
            to_name.contains('\\') || to_name.contains("::") || to_name.contains('/');

        let mut refs = if is_qualified {
            self.query_refs(
                "SELECT f.path, r.from_symbol, r.to_name, r.kind, r.line, r.col
                 FROM refs r JOIN files f ON r.from_file_id = f.id
                 WHERE r.to_name = ?1",
                params![to_name],
            )?
        } else {
            self.query_refs(
                "SELECT f.path, r.from_symbol, r.to_name, r.kind, r.line, r.col
                 FROM refs r JOIN files f ON r.from_file_id = f.id
                 WHERE r.to_name_tail = ?1 OR r.to_name = ?1",
                params![to_name],
            )?
        };

        if let Some(k) = kind {
            refs.retain(|r| r.kind == k);
        }
        Ok(refs)
    }

    pub fn references_in_file_range(
        &self,
        file_path: &str,
        line_start: u32,
        line_end: u32,
    ) -> Result<Vec<Reference>> {
        self.query_refs(
            "SELECT f.path, r.from_symbol, r.to_name, r.kind, r.line, r.col
             FROM refs r JOIN files f ON r.from_file_id = f.id
             WHERE f.path = ?1 AND r.line BETWEEN ?2 AND ?3
             ORDER BY r.line, r.col",
            params![file_path, line_start, line_end],
        )
    }

    fn query_refs(&self, sql: &str, params: impl rusqlite::Params) -> Result<Vec<Reference>> {
        // Cached: find_references runs in per-symbol loops during bundle
        // assembly; a handful of distinct SQL strings flow through here.
        let mut stmt = self.conn.prepare_cached(sql)?;
        let rows = stmt.query_map(params, |row| {
            let kind_str: String = row.get(3)?;
            Ok(Reference {
                from_file: row.get(0)?,
                from_symbol: row.get(1)?,
                to_name: row.get(2)?,
                kind: row_reference_kind(&kind_str)?,
                line: row.get(4)?,
                col: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// All distinct file → file import edges across the whole index.
    ///
    /// An edge `(a, b)` exists when file `a` has at least one `ref` of
    /// kind `import` / `include` / `inheritance` / `trait_use` whose
    /// `to_name` matches a symbol defined in file `b` (by short name or
    /// by qualified name — PHP fully-qualified, Python dotted, Rust
    /// path-style all land in `qualified_name`). Self-edges excluded.
    ///
    /// Used by the cycle-detection pass in `assess_risk_diff` and kept
    /// as a standalone method so other consumers (future RFC'd cross-repo
    /// graph merge, for instance) can reuse it. Scales with the refs
    /// table size: a typical mid-size TS project (~13k refs, ~1300 files)
    /// returns in tens of ms on a warm cache.
    pub fn enumerate_file_import_edges(&self) -> Result<Vec<(String, String)>> {
        let sql = r#"
            SELECT DISTINCT f_from.path, f_to.path
            FROM refs r
            JOIN files f_from ON r.from_file_id = f_from.id
            JOIN symbols s ON (
              s.qualified_name = r.to_name
              OR (
                s.name = r.to_name
                AND NOT EXISTS (
                  SELECT 1
                  FROM symbols s2
                  WHERE s2.name = r.to_name
                    AND s2.file_id <> s.file_id
                )
              )
            )
            JOIN files f_to ON s.file_id = f_to.id
            WHERE r.kind IN ('import', 'include', 'inheritance', 'trait_use')
              AND f_from.path <> f_to.path
        "#;
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn import_targets_for_file(&self, file_path: &str) -> Result<Vec<String>> {
        let sql = r#"
            SELECT DISTINCT f_to.path
            FROM refs r
            JOIN files f_from ON r.from_file_id = f_from.id
            JOIN symbols s ON (
              s.qualified_name = r.to_name
              OR (
                s.name = r.to_name
                AND NOT EXISTS (
                  SELECT 1
                  FROM symbols s2
                  WHERE s2.name = r.to_name
                    AND s2.file_id <> s.file_id
                )
              )
            )
            JOIN files f_to ON s.file_id = f_to.id
            WHERE r.kind IN ('import', 'include', 'inheritance', 'trait_use')
              AND f_from.path = ?1
              AND f_from.path <> f_to.path
        "#;
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt
            .query_map(params![file_path], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn import_sources_for_file(&self, file_path: &str) -> Result<Vec<String>> {
        let sql = r#"
            SELECT DISTINCT f_from.path
            FROM refs r
            JOIN files f_from ON r.from_file_id = f_from.id
            JOIN symbols s ON (
              s.qualified_name = r.to_name
              OR (
                s.name = r.to_name
                AND NOT EXISTS (
                  SELECT 1
                  FROM symbols s2
                  WHERE s2.name = r.to_name
                    AND s2.file_id <> s.file_id
                )
              )
            )
            JOIN files f_to ON s.file_id = f_to.id
            WHERE r.kind IN ('import', 'include', 'inheritance', 'trait_use')
              AND f_to.path = ?1
              AND f_from.path <> f_to.path
        "#;
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt
            .query_map(params![file_path], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Every import/include reference project-wide as `(from_path, to_name)`
    /// pairs, unresolved. One set-based query backing `list_dependencies`'
    /// path/module-import resolution sweep, which would otherwise be a
    /// per-file N+1 over `refs_outgoing_for_file_id`.
    pub fn import_include_refs_all(&self) -> Result<Vec<(String, String)>> {
        let sql = r#"
            SELECT DISTINCT f.path, r.to_name
            FROM refs r
            JOIN files f ON r.from_file_id = f.id
            WHERE r.kind IN ('import', 'include')
        "#;
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn import_cycle_cache_key(&self) -> Option<String> {
        self.conn
            .path()
            .filter(|p| !p.is_empty())
            .map(str::to_string)
    }

    pub fn import_cycle_validity_token(&self) -> Result<(i64, i64, i64, i64, i64, i64)> {
        let token = self.conn.query_row(
            "SELECT
                 (SELECT COUNT(*) FROM refs WHERE kind IN ('import', 'include', 'inheritance', 'trait_use')),
                 (SELECT COALESCE(MAX(id), 0) FROM refs WHERE kind IN ('import', 'include', 'inheritance', 'trait_use')),
                 (SELECT COALESCE(SUM(line + length(to_name) + length(kind)), 0)
                  FROM refs WHERE kind IN ('import', 'include', 'inheritance', 'trait_use')),
                 (SELECT COUNT(*) FROM symbols),
                 (SELECT COALESCE(MAX(id), 0) FROM symbols),
                 (SELECT COALESCE(SUM(file_id + line_start + line_end + length(qualified_name)), 0)
                  FROM symbols)",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )?;
        Ok(token)
    }

    /// Directed file→file import edges where BOTH endpoints are in `files`.
    /// Targeted variant of `enumerate_file_import_edges`: callers in the
    /// per-file risk path (e.g. ranking a break edge inside one import cycle)
    /// pass a small file set and avoid the full O(all-edges) enumeration, which
    /// would otherwise run once per scored file in the top-risk sweep.
    pub fn import_edges_within(&self, files: &[&str]) -> Result<Vec<(String, String)>> {
        if files.len() < 2 {
            return Ok(Vec::new());
        }
        let placeholders = std::iter::repeat_n("?", files.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            r#"
            SELECT DISTINCT f_from.path, f_to.path
            FROM refs r
            JOIN files f_from ON r.from_file_id = f_from.id
            JOIN symbols s ON (
              s.qualified_name = r.to_name
              OR (
                s.name = r.to_name
                AND NOT EXISTS (
                  SELECT 1
                  FROM symbols s2
                  WHERE s2.name = r.to_name
                    AND s2.file_id <> s.file_id
                )
              )
            )
            JOIN files f_to ON s.file_id = f_to.id
            WHERE r.kind IN ('import', 'include', 'inheritance', 'trait_use')
              AND f_from.path <> f_to.path
              AND f_from.path IN ({placeholders})
              AND f_to.path IN ({placeholders})
            "#
        );
        // The file list is bound twice — once per IN clause, in order.
        let binds: Vec<&str> = files.iter().copied().chain(files.iter().copied()).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn list_file_dependencies(&self, file_path: &str) -> Result<DependencyEntry> {
        if self.file_id_for_path(file_path)?.is_none() {
            return Ok(DependencyEntry {
                file_path: file_path.to_string(),
                found: false,
                note: Some(
                    "file is not indexed; verify the repo-relative path or run `codesage index`"
                        .to_string(),
                ),
                imports: Vec::new(),
                imported_by: Vec::new(),
            });
        }

        let mut imports_stmt = self.conn.prepare(
            "SELECT DISTINCT r.to_name
             FROM refs r JOIN files f ON r.from_file_id = f.id
             WHERE f.path = ?1 AND r.kind IN ('import', 'include')",
        )?;
        let imports: Vec<String> = imports_stmt
            .query_map(params![file_path], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut imported_by_stmt = self.conn.prepare(
            "SELECT DISTINCT f.path
             FROM refs r JOIN files f ON r.from_file_id = f.id
             WHERE r.to_name = ?1
               AND r.kind IN ('import', 'include')
               AND f.path <> ?1
             UNION
             SELECT DISTINCT f_from.path
             FROM refs r
             JOIN files f_from ON r.from_file_id = f_from.id
             JOIN symbols s ON (
               s.qualified_name = r.to_name
               OR (
                 s.name = r.to_name
                 AND NOT EXISTS (
                   SELECT 1
                   FROM symbols s2
                   WHERE s2.name = r.to_name
                     AND s2.file_id <> s.file_id
                 )
               )
             )
             JOIN files f_to ON s.file_id = f_to.id
             WHERE f_to.path = ?1
               AND r.kind IN ('import', 'include')
               AND f_from.path <> f_to.path
             ORDER BY 1",
        )?;
        let imported_by: Vec<String> = imported_by_stmt
            .query_map(params![file_path], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(DependencyEntry {
            file_path: file_path.to_string(),
            found: true,
            note: None,
            imports,
            imported_by,
        })
    }

    /// Batched lookup: returns a map from file_path → symbols for all distinct
    /// paths in one query. Empty entry for paths with no symbols.
    pub fn symbols_for_files(
        &self,
        file_paths: &[String],
    ) -> Result<std::collections::HashMap<String, Vec<Symbol>>> {
        use std::collections::HashMap;
        let mut out: HashMap<String, Vec<Symbol>> = HashMap::with_capacity(file_paths.len());
        if file_paths.is_empty() {
            return Ok(out);
        }
        for path in file_paths {
            out.entry(path.clone()).or_default();
        }
        let placeholders: Vec<String> = (1..=file_paths.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "SELECT s.name, s.qualified_name, s.kind, f.path,
                    s.line_start, s.line_end, s.col_start, s.col_end, s.rationale
             FROM symbols s JOIN files f ON s.file_id = f.id
             WHERE f.path IN ({})
             ORDER BY s.line_start",
            placeholders.join(",")
        );
        // Cached per placeholder-count. The bundle helpers call this with a
        // single path each (`annotate_with_symbols` on a one-element slice), so
        // the N=1 form is reused across every file added to a bundle within one
        // tool call instead of re-preparing each time.
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let params: Vec<&dyn rusqlite::types::ToSql> = file_paths
            .iter()
            .map(|p| p as &dyn rusqlite::types::ToSql)
            .collect();
        let rows = stmt.query_map(params.as_slice(), row_to_symbol)?;
        for sym_res in rows {
            let sym = sym_res?;
            out.entry(sym.file_path.clone()).or_default().push(sym);
        }
        Ok(out)
    }

    pub fn symbols_for_file(&self, file_path: &str) -> Result<Vec<Symbol>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.name, s.qualified_name, s.kind, f.path,
                    s.line_start, s.line_end, s.col_start, s.col_end, s.rationale
             FROM symbols s JOIN files f ON s.file_id = f.id
             WHERE f.path = ?1
             ORDER BY s.line_start",
        )?;
        let rows = stmt
            .query_map(params![file_path], row_to_symbol)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Every vec0 chunk table in the DB, across all models — not just the
    /// one this connection was opened for. A real chunk table is a
    /// `chunks_`-prefixed table with a vec0 `_info` shadow sibling; the
    /// sibling check filters out FTS sidecars and vec0's own shadow tables,
    /// which share the prefix. Same identification rule as the open-time
    /// chunk-table discovery in `db/mod.rs`.
    fn all_chunk_table_names(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT m.name FROM sqlite_master m
             WHERE m.type = 'table' AND m.name GLOB 'chunks_*'
               AND EXISTS (
                   SELECT 1 FROM sqlite_master aux
                   WHERE aux.type = 'table' AND aux.name = m.name || '_info'
               )
             ORDER BY m.name",
        )?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn table_exists(&self, name: &str) -> Result<bool> {
        let n: i64 = self
            .conn
            .prepare_cached(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            )?
            .query_row(params![name], |row| row.get(0))?;
        Ok(n > 0)
    }

    /// Remove one file from every per-path store: `files` (FK cascades cover
    /// symbols / refs / fingerprints / trust boundaries), semantic freshness,
    /// git history, feature membership, and EVERY model's chunk table plus
    /// its FTS sidecar. Sweeping all chunk tables — not just the active one —
    /// matters because a structural-only pass (`codesage index --no-semantic`)
    /// opens without a model, and a per-model delete would leave the removed
    /// file searchable until the next semantic sweep. Savepoint-wrapped so a
    /// mid-delete failure can't leave the removal half-applied; a savepoint
    /// (not a bare BEGIN) composes with a caller's outer transaction.
    pub fn remove_file(&self, path: &str) -> Result<()> {
        self.conn.execute_batch("SAVEPOINT remove_file")?;
        let result = (|| -> Result<()> {
            self.conn
                .prepare_cached("DELETE FROM files WHERE path = ?1")?
                .execute(params![path])?;
            self.conn
                .prepare_cached("DELETE FROM semantic_files WHERE path = ?1")?
                .execute(params![path])?;
            // git tables are path-keyed (not FK'd to `files`, because git-index can run
            // without a structural index), so cascade manually. Without this, deleted
            // files stay visible in `find_coupling` / `assess_risk` / future hotspots.
            self.conn
                .prepare_cached("DELETE FROM git_files WHERE path = ?1")?
                .execute(params![path])?;
            self.conn
                .prepare_cached("DELETE FROM git_co_changes WHERE file_a = ?1 OR file_b = ?1")?
                .execute(params![path])?;
            self.conn
                .prepare_cached("DELETE FROM feature_files WHERE path = ?1")?
                .execute(params![path])?;
            for table in self.all_chunk_table_names()? {
                let sql = format!(
                    "DELETE FROM \"{}\" WHERE file_path = ?1",
                    crate::schema::quote_ident(&table)
                );
                self.conn.prepare_cached(&sql)?.execute(params![path])?;
                let fts = crate::schema::fts_table_name(&table);
                if self.table_exists(&fts)? {
                    let fts_sql = format!(
                        "DELETE FROM \"{}\" WHERE file_path = ?1",
                        crate::schema::quote_ident(&fts)
                    );
                    self.conn.prepare_cached(&fts_sql)?.execute(params![path])?;
                }
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.conn.execute_batch("RELEASE remove_file")?;
                Ok(())
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK TO remove_file");
                if let Err(re) = self.conn.execute_batch("RELEASE remove_file") {
                    tracing::warn!(
                        error = %re,
                        "failed to release savepoint remove_file after rollback"
                    );
                }
                Err(e)
            }
        }
    }

    pub fn all_file_paths(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT path FROM files ORDER BY path")?;
        let paths = stmt
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(paths)
    }

    pub fn file_count(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))?;
        Ok(n as usize)
    }

    /// Indexed file count per language, descending. Aggregated in SQL rather
    /// than by loading every row like `all_files_with_id_and_language`, because
    /// this runs on the empty-result annotation path where the caller already
    /// got nothing back and must not pay a full table read for the
    /// explanation.
    pub fn file_counts_by_language(&self) -> Result<Vec<(String, usize)>> {
        let mut stmt = self.conn.prepare(
            "SELECT language, COUNT(*) AS n FROM files
             GROUP BY language
             ORDER BY n DESC, language ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as usize))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn symbol_count(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM symbols", [], |row| row.get(0))?;
        Ok(n as usize)
    }

    pub fn reference_count(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM refs", [], |row| row.get(0))?;
        Ok(n as usize)
    }

    /// Count refs targeting each of the given short names in a single query.
    /// Matches against both `to_name` and `to_name_tail` so language-qualified
    /// callsites (`App\Foo::bar`, `pkg.foo.bar`) still resolve back to a
    /// short-name lookup — mirrors the unqualified branch of `find_references`.
    /// Names with no matching refs are returned with a count of 0 so the caller
    /// can rely on a complete keyset.
    pub fn reference_counts_for_names(
        &self,
        names: &[String],
    ) -> Result<std::collections::HashMap<String, u32>> {
        use std::collections::HashMap;
        let mut out: HashMap<String, u32> = HashMap::with_capacity(names.len());
        if names.is_empty() {
            return Ok(out);
        }
        for n in names {
            out.entry(n.clone()).or_insert(0);
        }
        let placeholders: Vec<String> = (1..=names.len()).map(|i| format!("?{i}")).collect();
        // Each ref contributes once per (name, count) — we resolve by short
        // name. A ref whose `to_name_tail` matches the queried short name
        // counts the same as a direct `to_name` match (the indexer keeps
        // tail in sync via `name_tail()`).
        // Count distinct source sites, not rows. One import statement can emit
        // two rows naming the same short name — `import Foo from "./Foo"`
        // stores the module (whose `to_name_tail` is `Foo`) and the binding
        // `Foo` — and counting both scored a single statement twice in the
        // top-symbol ranking. The two rows share a (file, line), so keying on
        // that collapses them. Known cost: two real calls on ONE line
        // (`Foo(); Foo();`, or minified source) collapse too, because `col` is
        // deliberately left out — including it would separate the module from
        // its binding again, since they sit at different columns of the same
        // statement. Under-counting compact source beats double-counting every
        // extensionless import.
        let sql = format!(
            "SELECT name, COUNT(*) AS c FROM (
                SELECT DISTINCT name, from_file_id, line FROM (
                    SELECT to_name AS name, from_file_id, line FROM refs
                    WHERE to_name IN ({ph})
                    UNION ALL
                    SELECT to_name_tail AS name, from_file_id, line FROM refs
                    WHERE to_name_tail IN ({ph}) AND to_name_tail <> to_name
                )
            ) GROUP BY name",
            ph = placeholders.join(",")
        );
        let mut stmt = self.conn.prepare(&sql)?;
        // Bind the same name list once: SQLite reuses the param indices across
        // both halves of the UNION because we use positional `?N` references.
        let params: Vec<&dyn rusqlite::types::ToSql> = names
            .iter()
            .map(|p| p as &dyn rusqlite::types::ToSql)
            .collect();
        let rows = stmt.query_map(params.as_slice(), |row| {
            let name: String = row.get(0)?;
            let c: i64 = row.get(1)?;
            Ok((name, c))
        })?;
        for r in rows {
            let (name, c) = r?;
            *out.entry(name).or_insert(0) += c.max(0) as u32;
        }
        Ok(out)
    }

    /// Enumerate all indexed files with their database id and parsed language,
    /// in stable `path` order. Used by re-derivation passes (trust boundaries,
    /// feature mappers) that need to walk every file without touching symbols
    /// or refs first.
    pub fn all_files_with_id_and_language(&self) -> Result<Vec<(i64, String, Language)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, path, language FROM files ORDER BY path")?;
        let rows = stmt
            .query_map([], row_to_file_lang)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Outgoing references for one file: `(to_name, kind)` rows from `refs`
    /// where `from_file_id = ?`. Used by the trust-boundary derivation pass
    /// to re-classify a file's boundaries from its already-extracted refs
    /// without re-parsing the source.
    pub fn refs_outgoing_for_file_id(&self, file_id: i64) -> Result<Vec<(String, ReferenceKind)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT to_name, kind FROM refs WHERE from_file_id = ?1")?;
        let rows = stmt
            .query_map(params![file_id], |row| {
                let to_name: String = row.get(0)?;
                let kind_str: String = row.get(1)?;
                let kind = row_reference_kind(&kind_str)?;
                Ok((to_name, kind))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Replace every `file_trust_boundaries` row for `file_id` with `tags`.
    /// Idempotent (writing the same set twice yields the same DB state). Use
    /// inside an `execute_batch` for indexer-time writes; the wrapper opens
    /// its own transaction so a partial failure rolls back cleanly.
    pub fn replace_file_trust_boundaries(
        &self,
        file_id: i64,
        tags: &[TrustBoundary],
    ) -> Result<()> {
        self.conn
            .execute_batch("SAVEPOINT replace_file_trust_boundaries")?;
        let result = (|| -> Result<()> {
            // prepare_cached: called once per file in the boundary-derivation
            // loop, same rationale as the index-loop inserts above.
            self.conn
                .prepare_cached("DELETE FROM file_trust_boundaries WHERE file_id = ?1")?
                .execute(params![file_id])?;
            // Stamp `boundaries_derived_at` on every write — including when
            // the derived set is empty — so a rule-clean file (no matches)
            // stays distinguishable from a never-derived one (zero matches
            // because derivation never ran).
            self.conn
                .prepare_cached(
                    "UPDATE files SET boundaries_derived_at = unixepoch() WHERE id = ?1",
                )?
                .execute(params![file_id])?;
            if tags.is_empty() {
                return Ok(());
            }
            let mut stmt = self.conn.prepare_cached(
                "INSERT OR IGNORE INTO file_trust_boundaries (file_id, boundary) VALUES (?1, ?2)",
            )?;
            for t in tags {
                stmt.execute(params![file_id, t.as_str()])?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.conn
                    .execute_batch("RELEASE replace_file_trust_boundaries")?;
                Ok(())
            }
            Err(e) => {
                let _ = self
                    .conn
                    .execute_batch("ROLLBACK TO replace_file_trust_boundaries");
                if let Err(re) = self
                    .conn
                    .execute_batch("RELEASE replace_file_trust_boundaries")
                {
                    tracing::warn!(
                        error = %re,
                        "failed to release savepoint replace_file_trust_boundaries after rollback"
                    );
                }
                Err(e)
            }
        }
    }

    /// Files that have never had their trust boundaries derived (or were
    /// indexed before the marker column existed). Empty when every
    /// indexed file has been derived at least once. Pair with
    /// `derive_for_files` for a targeted catch-up pass.
    pub fn files_pending_boundary_derivation(&self) -> Result<Vec<(i64, String, Language)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, language FROM files
             WHERE boundaries_derived_at = 0
             ORDER BY path",
        )?;
        let rows = stmt
            .query_map([], row_to_file_lang)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Count rows in `file_trust_boundaries` across the whole project.
    /// Cheap COUNT(*) for diagnostics; the targeted backfill uses
    /// `files_pending_boundary_derivation` instead.
    pub fn file_trust_boundary_count(&self) -> Result<usize> {
        let n: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM file_trust_boundaries", [], |r| {
                    r.get(0)
                })?;
        Ok(n as usize)
    }

    /// Per-boundary file counts across the whole project, descending by count.
    /// Backs the trust-boundary cluster summary in `project_overview`.
    pub fn trust_boundary_counts(&self) -> Result<Vec<(TrustBoundary, usize)>> {
        let mut stmt = self.conn.prepare(
            "SELECT boundary, COUNT(*) AS n FROM file_trust_boundaries
             GROUP BY boundary ORDER BY n DESC",
        )?;
        let rows: Vec<(TrustBoundary, usize)> = stmt
            .query_map([], |row| {
                let s: String = row.get(0)?;
                let n: i64 = row.get(1)?;
                let b = row_enum(&s, TrustBoundary::parse, "TrustBoundary")?;
                Ok((b, n as usize))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Trust boundaries for one file identified by repo-relative path. Empty
    /// when the file has no row (never derived, or genuinely no boundary
    /// signal). Sorted by enum discriminant for stable output.
    pub fn trust_boundaries_for_file_path(&self, path: &str) -> Result<Vec<TrustBoundary>> {
        let mut stmt = self.conn.prepare(
            "SELECT b.boundary FROM file_trust_boundaries b
             JOIN files f ON f.id = b.file_id
             WHERE f.path = ?1",
        )?;
        let rows: Vec<TrustBoundary> = stmt
            .query_map(params![path], |row| {
                let s: String = row.get(0)?;
                row_enum(&s, TrustBoundary::parse, "TrustBoundary")
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut tags = rows;
        tags.sort();
        tags.dedup();
        Ok(tags)
    }
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}
