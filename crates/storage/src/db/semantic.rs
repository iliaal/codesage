//! Chunk table + sqlite-vec KNN + fullscan search.

use anyhow::Result;
use rusqlite::params;

use super::Database;

#[derive(Debug, Clone)]
pub struct RawSearchRow {
    pub file_path: String,
    pub language: String,
    pub content: String,
    pub start_line: u32,
    pub end_line: u32,
    pub distance: f32,
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
            // Keep FTS5 rowid == vec0 id so the two tables join trivially on
            // rowid / id. If the FTS insert fails, surface the error rather
            // than silently diverging from the vec0 state.
            fts_stmt.execute(params![
                rowid, content, file_path, language, start_line, end_line
            ])?;
        }
        Ok(())
    }

    pub fn delete_chunks_for_file(&self, file_path: &str) -> Result<usize> {
        let sql = format!("DELETE FROM \"{}\" WHERE file_path = ?1", self.chunk_table);
        let count = self.conn.execute(&sql, params![file_path])?;
        let fts = self.fts_table();
        let fts_sql = format!("DELETE FROM \"{fts}\" WHERE file_path = ?1");
        // FTS5 DELETE by file_path relies on UNINDEXED columns being queryable
        // by value; unicode61 tokenizer persists the column verbatim so this
        // works. Errors bubble — silent FTS drift is the problem we're
        // preventing.
        let _ = self.conn.execute(&fts_sql, params![file_path])?;
        Ok(count)
    }

    pub fn search_knn(
        &self,
        embedding_bytes: &[u8],
        k: usize,
        language: Option<&str>,
    ) -> Result<Vec<RawSearchRow>> {
        let t = &self.chunk_table;
        if let Some(lang) = language {
            let sql = format!(
                "SELECT file_path, language, content, start_line, end_line, distance
                 FROM \"{t}\"
                 WHERE embedding MATCH ?1 AND k = ?2 AND language = ?3
                 ORDER BY distance"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt
                .query_map(params![embedding_bytes, k as i64, lang], |row| {
                    Ok(RawSearchRow {
                        file_path: row.get(0)?,
                        language: row.get(1)?,
                        content: row.get(2)?,
                        start_line: row.get(3)?,
                        end_line: row.get(4)?,
                        distance: row.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        } else {
            let sql = format!(
                "SELECT file_path, language, content, start_line, end_line, distance
                 FROM \"{t}\"
                 WHERE embedding MATCH ?1 AND k = ?2
                 ORDER BY distance"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt
                .query_map(params![embedding_bytes, k as i64], |row| {
                    Ok(RawSearchRow {
                        file_path: row.get(0)?,
                        language: row.get(1)?,
                        content: row.get(2)?,
                        start_line: row.get(3)?,
                        end_line: row.get(4)?,
                        distance: row.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        }
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

        if let Some(path_patterns) = paths
            && !path_patterns.is_empty()
        {
            let clauses: Vec<String> = path_patterns
                .iter()
                .enumerate()
                .map(|(i, _)| format!("file_path GLOB ?{}", param_values.len() + i + 1))
                .collect();
            conditions.push(format!("({})", clauses.join(" OR ")));
            for p in path_patterns {
                param_values.push(Box::new(p.to_string()));
            }
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
            .query_map(params_refs.as_slice(), |row| {
                Ok(RawSearchRow {
                    file_path: row.get(0)?,
                    language: row.get(1)?,
                    content: row.get(2)?,
                    start_line: row.get(3)?,
                    end_line: row.get(4)?,
                    distance: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn chunk_count(&self) -> Result<usize> {
        let sql = format!("SELECT COUNT(*) FROM \"{}\"", self.chunk_table);
        let n: i64 = self.conn.query_row(&sql, [], |row| row.get(0))?;
        Ok(n as usize)
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
    ) -> Result<Vec<RawSearchRow>> {
        let t = self.fts_table();
        let sql = if language.is_some() {
            format!(
                "SELECT file_path, language, content, start_line, end_line, bm25(\"{t}\") AS score
                 FROM \"{t}\"
                 WHERE \"{t}\" MATCH ?1 AND language = ?2
                 ORDER BY score LIMIT ?3"
            )
        } else {
            format!(
                "SELECT file_path, language, content, start_line, end_line, bm25(\"{t}\") AS score
                 FROM \"{t}\"
                 WHERE \"{t}\" MATCH ?1
                 ORDER BY score LIMIT ?2"
            )
        };
        let mut stmt = self.conn.prepare(&sql)?;
        let row_fn = |row: &rusqlite::Row<'_>| {
            Ok(RawSearchRow {
                file_path: row.get(0)?,
                language: row.get(1)?,
                content: row.get(2)?,
                start_line: row.get(3)?,
                end_line: row.get(4)?,
                distance: row.get::<_, f64>(5)? as f32,
            })
        };
        let rows: Vec<RawSearchRow> = if let Some(lang) = language {
            stmt.query_map(params![match_expr, lang, k as i64], row_fn)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            stmt.query_map(params![match_expr, k as i64], row_fn)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
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
            "SELECT name FROM sqlite_master
             WHERE type = 'table'
               AND name LIKE 'chunks\\_%' ESCAPE '\\'
               AND name NOT LIKE '%\\_auxiliary' ESCAPE '\\'
               AND name NOT LIKE '%\\_rowids' ESCAPE '\\'
               AND name NOT LIKE '%\\_info' ESCAPE '\\'
               AND name NOT LIKE '%\\_chunks' ESCAPE '\\'
               AND name NOT LIKE '%\\_vector\\_chunks%' ESCAPE '\\'
             ORDER BY name",
        )?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn drop_vec_table(&self, table_name: &str) -> Result<()> {
        if !table_name.starts_with("chunks_") {
            anyhow::bail!("refusing to drop non-chunks table: {table_name}");
        }
        let sql = format!("DROP TABLE IF EXISTS \"{table_name}\"");
        self.conn.execute(&sql, [])?;
        Ok(())
    }

    pub fn vacuum(&self) -> Result<()> {
        self.conn.execute_batch("VACUUM")?;
        Ok(())
    }

    pub fn chunks_for_file(&self, file_path: &str) -> Result<Vec<RawSearchRow>> {
        let sql = format!(
            "SELECT file_path, language, content, start_line, end_line
             FROM \"{}\"
             WHERE file_path = ?1
             ORDER BY start_line",
            self.chunk_table
        );
        let mut stmt = self.conn.prepare(&sql)?;
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
