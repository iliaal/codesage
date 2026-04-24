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
        let mut vec_stmt = self.conn.prepare(&sql)?;

        for (content, start_line, end_line, embedding) in chunks {
            let bytes = embedding_to_bytes(embedding);
            vec_stmt.execute(params![
                file_path, language, content, start_line, end_line, bytes
            ])?;
        }
        Ok(())
    }

    pub fn delete_chunks_for_file(&self, file_path: &str) -> Result<usize> {
        let sql = format!("DELETE FROM \"{}\" WHERE file_path = ?1", self.chunk_table);
        let count = self.conn.execute(&sql, params![file_path])?;
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
