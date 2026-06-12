//! Chunk table + sqlite-vec KNN + fullscan search.

use anyhow::Result;
use rusqlite::params;

use super::{Database, drop_chunk_table_group};

#[derive(Debug, Clone)]
pub struct RawSearchRow {
    pub file_path: String,
    pub language: String,
    pub content: String,
    pub start_line: u32,
    pub end_line: u32,
    pub distance: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemanticFreshness {
    pub indexed_files: usize,
    pub missing_files: usize,
    pub stale_files: usize,
}

impl SemanticFreshness {
    pub fn is_fresh(&self) -> bool {
        self.missing_files == 0 && self.stale_files == 0
    }
}

pub fn embedding_to_bytes(embedding: &[f32]) -> Vec<u8> {
    // LE-only: one memcpy instead of a per-element `to_le_bytes` loop. f32 has
    // no padding, so its byte layout is exactly `len * 4`. A compile_error on
    // unsupported endianness is intentional — the binary would silently produce
    // wrong vec0 bytes otherwise, and none of our supported targets are BE.
    #[cfg(not(target_endian = "little"))]
    compile_error!("embedding_to_bytes assumes little-endian f32 layout");

    let byte_len = std::mem::size_of_val(embedding);
    // SAFETY: `embedding` is a valid `&[f32]` for `byte_len` bytes; `f32` has
    // no padding and its in-memory layout equals its on-disk sqlite-vec layout
    // on little-endian targets. The resulting `&[u8]` is read-only and bounded
    // by `embedding`'s lifetime, which outlives the `to_vec` copy below.
    let bytes = unsafe { std::slice::from_raw_parts(embedding.as_ptr() as *const u8, byte_len) };
    bytes.to_vec()
}

/// Map a `(file_path, language, content, start_line, end_line, distance)` row —
/// the column order returned by the KNN, fullscan, and BM25 searches — into a
/// `RawSearchRow`. `distance` is read as `f64` and narrowed so a BM25 `score`
/// (a REAL) and a vec0 `distance` decode the same way.
fn row_to_raw_search(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawSearchRow> {
    Ok(RawSearchRow {
        file_path: row.get(0)?,
        language: row.get(1)?,
        content: row.get(2)?,
        start_line: row.get(3)?,
        end_line: row.get(4)?,
        distance: row.get::<_, f64>(5)? as f32,
    })
}

/// Append a `(file_path GLOB ?N OR …)` condition for the given path patterns,
/// binding each as a positional parameter after the ones already in
/// `param_values`. No-op on an empty slice. Shared by `search_fullscan` and
/// `search_bm25`, which build the identical clause.
fn push_path_glob(
    conditions: &mut Vec<String>,
    param_values: &mut Vec<Box<dyn rusqlite::types::ToSql>>,
    paths: &[&str],
) {
    if paths.is_empty() {
        return;
    }
    let clauses: Vec<String> = paths
        .iter()
        .enumerate()
        .map(|(i, _)| format!("file_path GLOB ?{}", param_values.len() + i + 1))
        .collect();
    conditions.push(format!("({})", clauses.join(" OR ")));
    for p in paths {
        param_values.push(Box::new(p.to_string()));
    }
}

impl Database {
    /// FTS5 sidecar name for the active chunk table. Kept private to the
    /// storage layer — callers should go through `search_bm25` /
    /// `token_doc_frequency` rather than querying by name directly.
    fn fts_table(&self) -> String {
        crate::schema::fts_table_name(&self.chunk_table)
    }

    pub fn insert_chunks(
        &self,
        file_path: &str,
        language: &str,
        chunks: &[(&str, u32, u32, &[f32])],
    ) -> Result<()> {
        // Each chunk inserts a vec0 row and a matching FTS5 row, keyed by the
        // same rowid. A failure between the two leaves vec0 with an orphaned
        // row that has no FTS sidecar entry — `repair_fts_sidecar` heals this
        // on the next open, but until then BM25 search misses the row. All
        // current callers wrap in `execute_batch`, but pulling the BEGIN /
        // COMMIT in here keeps the function safe against future direct use.
        // The savepoint is a no-op when the caller already opened a tx.
        self.conn.execute_batch("SAVEPOINT insert_chunks")?;
        let result = (|| -> Result<()> {
            let sql = format!(
                "INSERT INTO \"{}\"(file_path, language, content, start_line, end_line, embedding)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                self.chunk_table
            );
            let fts = self.fts_table();
            let fts_sql = format!(
                "INSERT INTO \"{fts}\"(rowid, content, file_path, language, start_line, end_line)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
            );
            let mut vec_stmt = self.conn.prepare(&sql)?;
            let mut fts_stmt = self.conn.prepare(&fts_sql)?;

            for (content, start_line, end_line, embedding) in chunks {
                let bytes = embedding_to_bytes(embedding);
                vec_stmt.execute(params![
                    file_path, language, content, start_line, end_line, bytes
                ])?;
                let rowid = self.conn.last_insert_rowid();
                fts_stmt.execute(params![
                    rowid, content, file_path, language, start_line, end_line
                ])?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.conn.execute_batch("RELEASE insert_chunks")?;
                Ok(())
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK TO insert_chunks");
                let _ = self.conn.execute_batch("RELEASE insert_chunks");
                Err(e)
            }
        }
    }

    pub fn delete_chunks_for_file(&self, file_path: &str) -> Result<usize> {
        self.conn.execute_batch("SAVEPOINT delete_chunks")?;
        let result = (|| -> Result<usize> {
            let sql = format!("DELETE FROM \"{}\" WHERE file_path = ?1", self.chunk_table);
            let count = self.conn.execute(&sql, params![file_path])?;
            let fts = self.fts_table();
            let fts_sql = format!("DELETE FROM \"{fts}\" WHERE file_path = ?1");
            self.conn.execute(&fts_sql, params![file_path])?;
            Ok(count)
        })();
        match result {
            Ok(count) => {
                self.conn.execute_batch("RELEASE delete_chunks")?;
                Ok(count)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK TO delete_chunks");
                let _ = self.conn.execute_batch("RELEASE delete_chunks");
                Err(e)
            }
        }
    }

    pub fn search_knn(
        &self,
        embedding_bytes: &[u8],
        k: usize,
        language: Option<&str>,
    ) -> Result<Vec<RawSearchRow>> {
        let t = &self.chunk_table;
        let lang_clause = if language.is_some() {
            " AND language = ?3"
        } else {
            ""
        };
        let sql = format!(
            "SELECT file_path, language, content, start_line, end_line, distance
             FROM \"{t}\"
             WHERE embedding MATCH ?1 AND k = ?2{lang_clause}
             ORDER BY distance"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = if let Some(lang) = language {
            stmt.query_map(params![embedding_bytes, k as i64, lang], row_to_raw_search)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            stmt.query_map(params![embedding_bytes, k as i64], row_to_raw_search)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(rows)
    }

    pub fn search_fullscan(
        &self,
        embedding_bytes: &[u8],
        limit: usize,
        offset: usize,
        languages: Option<&[&str]>,
        paths: Option<&[&str]>,
    ) -> Result<Vec<RawSearchRow>> {
        let t = &self.chunk_table;
        let mut conditions = Vec::new();
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        param_values.push(Box::new(embedding_bytes.to_vec()));

        if let Some(langs) = languages
            && !langs.is_empty()
        {
            let placeholders: Vec<String> = langs
                .iter()
                .enumerate()
                .map(|(i, _)| format!("?{}", param_values.len() + i + 1))
                .collect();
            conditions.push(format!("language IN ({})", placeholders.join(",")));
            for lang in langs {
                param_values.push(Box::new(lang.to_string()));
            }
        }

        if let Some(path_patterns) = paths {
            push_path_glob(&mut conditions, &mut param_values, path_patterns);
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };

        let sql = format!(
            "SELECT file_path, language, content, start_line, end_line,
                    vec_distance_L2(embedding, ?1) as distance
             FROM \"{t}\"
             {where_clause}
             ORDER BY distance
             LIMIT ?{} OFFSET ?{}",
            param_values.len() + 1,
            param_values.len() + 2,
        );
        param_values.push(Box::new(limit as i64));
        param_values.push(Box::new(offset as i64));

        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            param_values.iter().map(|p| p.as_ref()).collect();

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_refs.as_slice(), row_to_raw_search)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn chunk_count(&self) -> Result<usize> {
        let sql = format!("SELECT COUNT(*) FROM \"{}\"", self.chunk_table);
        let n: i64 = self.conn.query_row(&sql, [], |row| row.get(0))?;
        Ok(n as usize)
    }

    /// Distinct files with at least one semantic chunk indexed, across all
    /// model chunk tables. Model-agnostic (reads `semantic_files`, a shared
    /// table) so it works on a plain structural handle. Backs the semantic
    /// freshness signal in `project_overview`.
    pub fn semantic_file_count(&self) -> Result<usize> {
        let n: i64 =
            self.conn
                .query_row("SELECT COUNT(DISTINCT path) FROM semantic_files", [], |r| {
                    r.get(0)
                })?;
        Ok(n as usize)
    }

    pub fn all_semantic_file_hashes(&self) -> Result<std::collections::HashMap<String, String>> {
        if self.chunk_table.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let mut stmt = self
            .conn
            .prepare("SELECT path, content_hash FROM semantic_files WHERE chunk_table = ?1")?;
        let rows = stmt
            .query_map(params![&self.chunk_table], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<std::collections::HashMap<_, _>>>()?;
        Ok(rows)
    }

    pub fn upsert_semantic_file_hash(&self, path: &str, content_hash: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO semantic_files (chunk_table, path, content_hash, indexed_at)
             VALUES (?1, ?2, ?3, unixepoch())
             ON CONFLICT(chunk_table, path) DO UPDATE SET
                 content_hash = excluded.content_hash,
                 indexed_at = excluded.indexed_at",
            params![&self.chunk_table, path, content_hash],
        )?;
        Ok(())
    }

    pub fn delete_semantic_file_hash(&self, path: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM semantic_files WHERE chunk_table = ?1 AND path = ?2",
            params![&self.chunk_table, path],
        )?;
        Ok(())
    }

    pub fn semantic_freshness(&self) -> Result<Option<SemanticFreshness>> {
        if self.chunk_table.is_empty() {
            return Ok(None);
        }

        let indexed_files: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM semantic_files WHERE chunk_table = ?1",
            params![&self.chunk_table],
            |row| row.get(0),
        )?;
        let (missing_files, stale_files): (i64, i64) = self.conn.query_row(
            "SELECT
                 COALESCE(SUM(CASE WHEN sf.path IS NULL THEN 1 ELSE 0 END), 0),
                 COALESCE(SUM(CASE
                     WHEN sf.path IS NOT NULL AND sf.content_hash <> f.content_hash THEN 1
                     ELSE 0
                 END), 0)
             FROM files f
             LEFT JOIN semantic_files sf
               ON sf.chunk_table = ?1 AND sf.path = f.path",
            params![&self.chunk_table],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;

        Ok(Some(SemanticFreshness {
            indexed_files: indexed_files as usize,
            missing_files: missing_files as usize,
            stale_files: stale_files as usize,
        }))
    }

    /// BM25 search over the FTS5 sidecar of the active chunk table. Returns
    /// the top-N rows by FTS5's built-in BM25 ranking (lower = better), in
    /// the same `RawSearchRow` shape as `search_knn` for easy RRF fusion.
    /// `distance` carries the raw BM25 score; consumers should convert via
    /// rank-position when fusing, not raw value.
    ///
    /// Query must be an FTS5 MATCH expression; pass pre-escaped. Callers that
    /// build a query from user input should go through a helper like
    /// `build_fts_match_query` to quote identifiers safely.
    pub fn search_bm25(
        &self,
        match_expr: &str,
        k: usize,
        language: Option<&str>,
        paths: Option<&[&str]>,
    ) -> Result<Vec<RawSearchRow>> {
        let t = self.fts_table();
        let mut conditions = vec![format!("\"{t}\" MATCH ?1")];
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> =
            vec![Box::new(match_expr.to_string())];

        if let Some(lang) = language {
            let idx = param_values.len() + 1;
            conditions.push(format!("language = ?{idx}"));
            param_values.push(Box::new(lang.to_string()));
        }

        if let Some(path_patterns) = paths {
            push_path_glob(&mut conditions, &mut param_values, path_patterns);
        }

        let limit_idx = param_values.len() + 1;
        let sql = format!(
            "SELECT file_path, language, content, start_line, end_line, bm25(\"{t}\") AS score
             FROM \"{t}\"
             WHERE {}
             ORDER BY score LIMIT ?{limit_idx}",
            conditions.join(" AND ")
        );
        param_values.push(Box::new(k as i64));

        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            param_values.iter().map(|p| p.as_ref()).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows: Vec<RawSearchRow> = stmt
            .query_map(params_refs.as_slice(), row_to_raw_search)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Doc frequency of `token` in the active FTS5 sidecar, as a fraction
    /// `(docs_with_token, total_docs)`. Used by the hybrid query gate to
    /// decide whether a query contains a "rare" literal worth a BM25 boost.
    /// Returns `(0, total)` when the token is absent or FTS is empty.
    ///
    /// Uses the `fts5vocab` table in `row` mode: rows are one per term with
    /// `doc` counting the number of distinct docs containing the term.
    pub fn token_doc_frequency(&self, token: &str) -> Result<(u64, u64)> {
        let total = self.chunk_count()? as u64;
        if total == 0 {
            return Ok((0, 0));
        }
        let vocab = format!("{}_vocab", self.fts_table());
        // Create the vocab shadow if it doesn't exist. FTS5 vocab tables are
        // virtual; creating them is idempotent and only records the shape,
        // no data copy.
        self.conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS \"{vocab}\" USING fts5vocab(\"{}\", row);",
            self.fts_table()
        ))?;
        let sql = format!("SELECT doc FROM \"{vocab}\" WHERE term = ?1");
        let doc: Option<i64> = self
            .conn
            .query_row(&sql, params![token.to_lowercase()], |r| r.get(0))
            .ok();
        Ok((doc.unwrap_or(0) as u64, total))
    }

    /// Chunk count across **every** vec0 chunk table in the DB, not just the
    /// currently-selected model's. Used by `codesage status` where the caller
    /// opens via [`Database::open`] (no chunk table selected) and just wants a
    /// total index size. Returns `Ok(0)` on a DB that has never run a semantic
    /// index.
    pub fn total_chunk_count(&self) -> Result<usize> {
        let tables = self.list_vec_tables()?;
        let mut total: i64 = 0;
        for t in &tables {
            let sql = format!("SELECT COUNT(*) FROM \"{t}\"");
            let n: i64 = self.conn.query_row(&sql, [], |row| row.get(0))?;
            total += n;
        }
        Ok(total as usize)
    }

    pub fn list_vec_tables(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.name FROM sqlite_master m
             WHERE m.type = 'table'
               AND m.name LIKE 'chunks\\_%' ESCAPE '\\'
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

    pub fn drop_vec_table(&self, table_name: &str) -> Result<()> {
        drop_chunk_table_group(&self.conn, table_name)
    }

    pub fn vacuum(&self) -> Result<()> {
        self.conn.execute_batch("VACUUM")?;
        Ok(())
    }

    pub fn chunks_for_file(&self, file_path: &str) -> Result<Vec<RawSearchRow>> {
        if self.chunk_table.is_empty() {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT file_path, language, content, start_line, end_line
             FROM \"{}\"
             WHERE file_path = ?1
             ORDER BY start_line",
            self.chunk_table
        );
        // Cached: chunks_for_file is called once per file during bundle
        // assembly (entry + owned + context). The table name is fixed per
        // connection, so the SQL string is stable and the cached statement is
        // reused across all files in one feature_bundle / export_context call.
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt
            .query_map(params![file_path], |row| {
                Ok(RawSearchRow {
                    file_path: row.get(0)?,
                    language: row.get(1)?,
                    content: row.get(2)?,
                    start_line: row.get(3)?,
                    end_line: row.get(4)?,
                    distance: 0.0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn all_chunk_file_paths(&self) -> Result<Vec<String>> {
        let sql = format!(
            "SELECT DISTINCT file_path FROM \"{}\" ORDER BY file_path",
            self.chunk_table
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let paths = stmt
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(paths)
    }
}
