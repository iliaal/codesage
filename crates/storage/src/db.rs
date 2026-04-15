use std::path::Path;

use anyhow::Result;
use codesage_protocol::{DependencyEntry, FileInfo, Reference, ReferenceKind, Symbol, SymbolKind};
use rusqlite::{Connection, params};

use crate::schema::{ensure_chunk_table, init_db, init_vec_extension, model_table_name, name_tail};

pub use codesage_protocol::DEFAULT_EMBEDDING_DIM;

pub struct Database {
    conn: Connection,
    chunk_table: String,
}

impl Database {
    /// Open a DB for read-only (structural) queries. No chunk/vec table is created;
    /// semantic queries will fail until `open_for_model` is used instead.
    pub fn open(path: &Path) -> Result<Self> {
        init_vec_extension();
        let conn = Connection::open(path)?;
        init_db(&conn)?;
        Ok(Database {
            conn,
            chunk_table: String::new(),
        })
    }

    pub fn open_for_model(path: &Path, model: &str, dim: usize) -> Result<Self> {
        init_vec_extension();
        let conn = Connection::open(path)?;
        init_db(&conn)?;
        let chunk_table = model_table_name(model, dim);
        ensure_chunk_table(&conn, &chunk_table, dim)?;
        Ok(Database { conn, chunk_table })
    }

    pub fn open_in_memory() -> Result<Self> {
        init_vec_extension();
        let conn = Connection::open_in_memory()?;
        init_db(&conn)?;
        let chunk_table = model_table_name("default", DEFAULT_EMBEDDING_DIM);
        ensure_chunk_table(&conn, &chunk_table, DEFAULT_EMBEDDING_DIM)?;
        Ok(Database { conn, chunk_table })
    }

    pub fn chunk_table_name(&self) -> &str {
        &self.chunk_table
    }

    pub fn execute_batch(&self, f: impl FnOnce(&Self) -> Result<()>) -> Result<()> {
        self.conn.execute_batch("BEGIN")?;
        match f(self) {
            Ok(()) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    pub fn upsert_file(&self, file: &FileInfo) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO files (path, language, content_hash, indexed_at)
             VALUES (?1, ?2, ?3, unixepoch())
             ON CONFLICT(path) DO UPDATE SET
               language = excluded.language,
               content_hash = excluded.content_hash,
               indexed_at = excluded.indexed_at",
            params![file.path, file.language.as_str(), file.content_hash],
        )?;

        let file_id: i64 = self.conn.query_row(
            "SELECT id FROM files WHERE path = ?1",
            params![file.path],
            |row| row.get(0),
        )?;

        self.conn
            .execute("DELETE FROM symbols WHERE file_id = ?1", params![file_id])?;
        self.conn
            .execute("DELETE FROM refs WHERE from_file_id = ?1", params![file_id])?;

        Ok(file_id)
    }

    pub fn insert_symbols(&self, file_id: i64, symbols: &[Symbol]) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "INSERT INTO symbols (file_id, name, qualified_name, kind, line_start, line_end, col_start, col_end)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;

        for s in symbols {
            stmt.execute(params![
                file_id,
                s.name,
                s.qualified_name,
                s.kind.as_str(),
                s.line_start,
                s.line_end,
                s.col_start,
                s.col_end,
            ])?;
        }
        Ok(())
    }

    pub fn insert_references(&self, file_id: i64, refs: &[Reference]) -> Result<()> {
        let mut stmt = self.conn.prepare(
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

    pub fn find_symbols(&self, name: &str, kind: Option<SymbolKind>) -> Result<Vec<Symbol>> {
        let (sql, is_qualified) = if name.contains('\\') || name.contains('.') {
            ("SELECT s.name, s.qualified_name, s.kind, f.path, s.line_start, s.line_end, s.col_start, s.col_end
              FROM symbols s JOIN files f ON s.file_id = f.id
              WHERE s.qualified_name LIKE ?1", true)
        } else {
            ("SELECT s.name, s.qualified_name, s.kind, f.path, s.line_start, s.line_end, s.col_start, s.col_end
              FROM symbols s JOIN files f ON s.file_id = f.id
              WHERE s.name = ?1", false)
        };

        let search_term = if is_qualified {
            format!("%{name}%")
        } else {
            name.to_string()
        };

        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params![search_term], |row| {
            let kind_str: String = row.get(2)?;
            Ok(Symbol {
                name: row.get(0)?,
                qualified_name: row.get(1)?,
                kind: SymbolKind::parse(&kind_str).unwrap_or(SymbolKind::Function),
                file_path: row.get(3)?,
                line_start: row.get(4)?,
                line_end: row.get(5)?,
                col_start: row.get(6)?,
                col_end: row.get(7)?,
            })
        })?;

        let mut symbols: Vec<Symbol> = rows.filter_map(|r| r.ok()).collect();
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

    fn query_refs(&self, sql: &str, params: impl rusqlite::Params) -> Result<Vec<Reference>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params, |row| {
            let kind_str: String = row.get(3)?;
            Ok(Reference {
                from_file: row.get(0)?,
                from_symbol: row.get(1)?,
                to_name: row.get(2)?,
                kind: ReferenceKind::parse(&kind_str).unwrap_or(ReferenceKind::Call),
                line: row.get(4)?,
                col: row.get(5)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn list_file_dependencies(&self, file_path: &str) -> Result<DependencyEntry> {
        let mut imports_stmt = self.conn.prepare(
            "SELECT DISTINCT r.to_name
             FROM refs r JOIN files f ON r.from_file_id = f.id
             WHERE f.path = ?1 AND r.kind IN ('import', 'include')",
        )?;
        let imports: Vec<String> = imports_stmt
            .query_map(params![file_path], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();

        let mut imported_by_stmt = self.conn.prepare(
            "SELECT DISTINCT f.path
             FROM refs r JOIN files f ON r.from_file_id = f.id
             WHERE r.to_name = ?1 AND r.kind IN ('import', 'include')",
        )?;
        let imported_by: Vec<String> = imported_by_stmt
            .query_map(params![file_path], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();

        Ok(DependencyEntry {
            file_path: file_path.to_string(),
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
                    s.line_start, s.line_end, s.col_start, s.col_end
             FROM symbols s JOIN files f ON s.file_id = f.id
             WHERE f.path IN ({})
             ORDER BY s.line_start",
            placeholders.join(",")
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::types::ToSql> = file_paths
            .iter()
            .map(|p| p as &dyn rusqlite::types::ToSql)
            .collect();
        let rows = stmt.query_map(params.as_slice(), |row| {
            let kind_str: String = row.get(2)?;
            Ok(Symbol {
                name: row.get(0)?,
                qualified_name: row.get(1)?,
                kind: SymbolKind::parse(&kind_str).unwrap_or(SymbolKind::Function),
                file_path: row.get(3)?,
                line_start: row.get(4)?,
                line_end: row.get(5)?,
                col_start: row.get(6)?,
                col_end: row.get(7)?,
            })
        })?;
        for sym in rows.flatten() {
            out.entry(sym.file_path.clone()).or_default().push(sym);
        }
        Ok(out)
    }

    pub fn symbols_for_file(&self, file_path: &str) -> Result<Vec<Symbol>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.name, s.qualified_name, s.kind, f.path,
                    s.line_start, s.line_end, s.col_start, s.col_end
             FROM symbols s JOIN files f ON s.file_id = f.id
             WHERE f.path = ?1
             ORDER BY s.line_start",
        )?;
        let rows = stmt
            .query_map(params![file_path], |row| {
                let kind_str: String = row.get(2)?;
                Ok(Symbol {
                    name: row.get(0)?,
                    qualified_name: row.get(1)?,
                    kind: SymbolKind::parse(&kind_str).unwrap_or(SymbolKind::Function),
                    file_path: row.get(3)?,
                    line_start: row.get(4)?,
                    line_end: row.get(5)?,
                    col_start: row.get(6)?,
                    col_end: row.get(7)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    pub fn remove_file(&self, path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM files WHERE path = ?1", params![path])?;
        Ok(())
    }

    pub fn all_file_paths(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT path FROM files ORDER BY path")?;
        let paths = stmt
            .query_map([], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(paths)
    }

    pub fn file_count(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))?;
        Ok(n as usize)
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

    pub fn insert_chunks(
        &self,
        file_path: &str,
        language: &str,
        chunks: &[(String, u32, u32, Vec<f32>)],
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
                .filter_map(|r| r.ok())
                .collect();
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
                .filter_map(|r| r.ok())
                .collect();
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
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    pub fn chunk_count(&self) -> Result<usize> {
        let sql = format!("SELECT COUNT(*) FROM \"{}\"", self.chunk_table);
        let n: i64 = self.conn.query_row(&sql, [], |row| row.get(0))?;
        Ok(n as usize)
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
            .filter_map(|r| r.ok())
            .collect();
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
            .filter_map(|r| r.ok())
            .collect();
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
            .filter_map(|r| r.ok())
            .collect();
        Ok(paths)
    }

    // ----- git history (V2b) -----

    /// UPSERT a git_files row. Re-running the indexer must replace prior values, not stack.
    pub fn upsert_git_file(
        &self,
        path: &str,
        churn_score: f64,
        fix_count: u32,
        total_commits: u32,
        last_commit_at: Option<i64>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO git_files (path, churn_score, fix_count, total_commits, last_commit_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(path) DO UPDATE SET
                 churn_score = excluded.churn_score,
                 fix_count = excluded.fix_count,
                 total_commits = excluded.total_commits,
                 last_commit_at = excluded.last_commit_at,
                 indexed_at = unixepoch()",
            rusqlite::params![path, churn_score, fix_count, total_commits, last_commit_at],
        )?;
        Ok(())
    }

    /// UPSERT a co-change pair. Caller must ensure file_a < file_b lexicographically so
    /// each pair is stored exactly once.
    pub fn upsert_git_co_change(
        &self,
        file_a: &str,
        file_b: &str,
        weight: f64,
        count: u32,
        last_observed_at: Option<i64>,
    ) -> Result<()> {
        debug_assert!(
            file_a < file_b,
            "co-change pair must be sorted: {file_a} >= {file_b}"
        );
        self.conn.execute(
            "INSERT INTO git_co_changes (file_a, file_b, weight, count, last_observed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(file_a, file_b) DO UPDATE SET
                 weight = excluded.weight,
                 count = excluded.count,
                 last_observed_at = excluded.last_observed_at",
            rusqlite::params![file_a, file_b, weight, count, last_observed_at],
        )?;
        Ok(())
    }

    /// Wipe all git data. Indexer should call before a fresh full pass to avoid stale rows
    /// for files that were renamed/deleted.
    pub fn clear_git_data(&self) -> Result<()> {
        self.conn.execute("DELETE FROM git_files", [])?;
        self.conn.execute("DELETE FROM git_co_changes", [])?;
        self.conn.execute("DELETE FROM git_index_state", [])?;
        Ok(())
    }

    /// Return (last_sha, last_indexed_at_unix) if an incremental state exists.
    pub fn get_git_index_state(&self) -> Result<Option<(String, i64)>> {
        let row = self.conn.query_row(
            "SELECT last_sha, last_indexed_at FROM git_index_state WHERE id = 1",
            [],
            |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, i64>(1)?)),
        );
        match row {
            Ok((Some(sha), at)) if !sha.is_empty() => Ok(Some((sha, at))),
            Ok(_) => Ok(None),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// All file paths in `git_files` whose path begins with `prefix`. Used by
    /// the test recommender to enumerate Rust integration tests under a crate's
    /// `tests/` directory, where there's no per-file naming convention to lean on.
    pub fn git_files_with_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM git_files WHERE path LIKE ?1 ORDER BY path")?;
        let pattern = format!("{prefix}%");
        let rows: Vec<String> = stmt
            .query_map(rusqlite::params![pattern], |r| r.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// Record the commit SHA we just indexed up to. indexed_at stamped with unixepoch().
    pub fn set_git_index_state(&self, sha: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO git_index_state (id, last_sha, last_indexed_at)
             VALUES (1, ?1, unixepoch())
             ON CONFLICT(id) DO UPDATE SET
                 last_sha = excluded.last_sha,
                 last_indexed_at = excluded.last_indexed_at",
            rusqlite::params![sha],
        )?;
        Ok(())
    }

    /// Apply a global multiplicative decay factor to existing churn and co-change weights.
    /// Used in incremental mode to age rows to "now" before adding new-commit deltas.
    /// This is exact for exponential decay: all historic commits multiplied by the same
    /// factor equals ageing each by the same extra seconds.
    pub fn scale_git_decay(&self, factor: f64) -> Result<()> {
        self.conn.execute(
            "UPDATE git_files SET churn_score = churn_score * ?1",
            rusqlite::params![factor],
        )?;
        self.conn.execute(
            "UPDATE git_co_changes SET weight = weight * ?1",
            rusqlite::params![factor],
        )?;
        Ok(())
    }

    /// Additive upsert for a git_files row. Unlike `upsert_git_file`, this ADDs to the
    /// existing counters rather than replacing them. Incremental mode uses this.
    pub fn incr_git_file(
        &self,
        path: &str,
        add_churn: f64,
        add_fix: u32,
        add_commits: u32,
        last_commit_at: Option<i64>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO git_files (path, churn_score, fix_count, total_commits, last_commit_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(path) DO UPDATE SET
                 churn_score = churn_score + excluded.churn_score,
                 fix_count = fix_count + excluded.fix_count,
                 total_commits = total_commits + excluded.total_commits,
                 last_commit_at = MAX(COALESCE(last_commit_at, 0), COALESCE(excluded.last_commit_at, 0)),
                 indexed_at = unixepoch()",
            rusqlite::params![path, add_churn, add_fix, add_commits, last_commit_at],
        )?;
        Ok(())
    }

    /// Additive upsert for a co-change pair.
    pub fn incr_git_co_change(
        &self,
        file_a: &str,
        file_b: &str,
        add_weight: f64,
        add_count: u32,
        last_observed_at: Option<i64>,
    ) -> Result<()> {
        debug_assert!(
            file_a < file_b,
            "co-change pair must be sorted: {file_a} >= {file_b}"
        );
        self.conn.execute(
            "INSERT INTO git_co_changes (file_a, file_b, weight, count, last_observed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(file_a, file_b) DO UPDATE SET
                 weight = weight + excluded.weight,
                 count = count + excluded.count,
                 last_observed_at = MAX(COALESCE(last_observed_at, 0), COALESCE(excluded.last_observed_at, 0))",
            rusqlite::params![file_a, file_b, add_weight, add_count, last_observed_at],
        )?;
        Ok(())
    }

    /// True if a co-change pair already exists in the DB. `file_a` must be the lexicographic
    /// lower end. Used by incremental indexing to decide whether a sub-threshold pair should
    /// be upserted (existing pairs keep accumulating) or dropped (new noise below threshold).
    pub fn co_change_pair_exists(&self, file_a: &str, file_b: &str) -> Result<bool> {
        debug_assert!(file_a < file_b);
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM git_co_changes WHERE file_a = ?1 AND file_b = ?2",
            rusqlite::params![file_a, file_b],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Fetch git_files row for one path, if present.
    pub fn git_file(&self, path: &str) -> Result<Option<GitFileRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, churn_score, fix_count, total_commits, last_commit_at
             FROM git_files WHERE path = ?1",
        )?;
        let mut rows = stmt.query(rusqlite::params![path])?;
        if let Some(row) = rows.next()? {
            Ok(Some(GitFileRow {
                path: row.get(0)?,
                churn_score: row.get(1)?,
                fix_count: row.get::<_, i64>(2)? as u32,
                total_commits: row.get::<_, i64>(3)? as u32,
                last_commit_at: row.get(4)?,
            }))
        } else {
            Ok(None)
        }
    }

    /// Top N files that historically co-change with `path`. Returns the OTHER file in each
    /// pair, weight-sorted descending.
    pub fn co_changes_for(&self, path: &str, limit: usize) -> Result<Vec<CoChangeRow>> {
        // Pair is stored with file_a < file_b. For a given path, results live on
        // either side, so query both columns and union-rank.
        let mut stmt = self.conn.prepare(
            "SELECT other, weight, count, last_observed_at FROM (
                 SELECT file_b AS other, weight, count, last_observed_at
                 FROM git_co_changes WHERE file_a = ?1
                 UNION ALL
                 SELECT file_a AS other, weight, count, last_observed_at
                 FROM git_co_changes WHERE file_b = ?1
             ) ORDER BY weight DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![path, limit as i64], |row| {
                Ok(CoChangeRow {
                    file: row.get(0)?,
                    weight: row.get(1)?,
                    count: row.get::<_, i64>(2)? as u32,
                    last_observed_at: row.get(3)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// Compute churn percentile for a single path using all git_files churn scores.
    /// Returns 0.0..=1.0 where 1.0 means highest churn observed.
    pub fn churn_percentile(&self, path: &str) -> Result<f64> {
        let target: Option<f64> = self
            .conn
            .query_row(
                "SELECT churn_score FROM git_files WHERE path = ?1",
                rusqlite::params![path],
                |r| r.get(0),
            )
            .ok();
        let Some(target) = target else {
            return Ok(0.0);
        };
        let (lower, total): (i64, i64) = self.conn.query_row(
            "SELECT
                 SUM(CASE WHEN churn_score <= ?1 THEN 1 ELSE 0 END),
                 COUNT(*)
             FROM git_files",
            rusqlite::params![target],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if total == 0 {
            return Ok(0.0);
        }
        Ok(lower as f64 / total as f64)
    }
}

#[derive(Debug, Clone)]
pub struct GitFileRow {
    pub path: String,
    pub churn_score: f64,
    pub fix_count: u32,
    pub total_commits: u32,
    pub last_commit_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct CoChangeRow {
    pub file: String,
    pub weight: f64,
    pub count: u32,
    pub last_observed_at: Option<i64>,
}

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
    let mut bytes = Vec::with_capacity(embedding.len() * 4);
    for &v in embedding {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use codesage_protocol::Language;

    fn make_file(path: &str) -> FileInfo {
        FileInfo {
            path: path.to_string(),
            language: Language::Php,
            content_hash: "abc123".to_string(),
        }
    }

    fn make_symbol(name: &str, kind: SymbolKind) -> Symbol {
        Symbol {
            name: name.to_string(),
            qualified_name: name.to_string(),
            kind,
            file_path: "test.php".to_string(),
            line_start: 1,
            line_end: 5,
            col_start: 0,
            col_end: 0,
        }
    }

    fn make_reference(to_name: &str, kind: ReferenceKind) -> Reference {
        Reference {
            from_file: "test.php".to_string(),
            from_symbol: None,
            to_name: to_name.to_string(),
            kind,
            line: 1,
            col: 0,
        }
    }

    #[test]
    fn insert_and_query_symbols() {
        let db = Database::open_in_memory().unwrap();
        let file_id = db.upsert_file(&make_file("test.php")).unwrap();
        let symbols = vec![
            make_symbol("Foo", SymbolKind::Class),
            make_symbol("bar", SymbolKind::Function),
        ];
        db.insert_symbols(file_id, &symbols).unwrap();
        let found = db.find_symbols("Foo", None).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "Foo");
    }

    #[test]
    fn insert_and_query_references() {
        let db = Database::open_in_memory().unwrap();
        let file_id = db.upsert_file(&make_file("test.php")).unwrap();
        let refs = vec![make_reference("SomeClass", ReferenceKind::Import)];
        db.insert_references(file_id, &refs).unwrap();
        let found = db.find_references("SomeClass", None).unwrap();
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn upsert_clears_old_data() {
        let db = Database::open_in_memory().unwrap();
        let file_id = db.upsert_file(&make_file("test.php")).unwrap();
        db.insert_symbols(file_id, &[make_symbol("Old", SymbolKind::Function)])
            .unwrap();
        let file_id2 = db
            .upsert_file(&FileInfo {
                path: "test.php".to_string(),
                language: Language::Php,
                content_hash: "new_hash".to_string(),
            })
            .unwrap();
        db.insert_symbols(file_id2, &[make_symbol("New", SymbolKind::Function)])
            .unwrap();
        assert!(db.find_symbols("Old", None).unwrap().is_empty());
        assert_eq!(db.find_symbols("New", None).unwrap().len(), 1);
    }

    #[test]
    fn remove_file_cascades() {
        let db = Database::open_in_memory().unwrap();
        let file_id = db.upsert_file(&make_file("test.php")).unwrap();
        db.insert_symbols(file_id, &[make_symbol("Foo", SymbolKind::Class)])
            .unwrap();
        db.insert_references(file_id, &[make_reference("Bar", ReferenceKind::Call)])
            .unwrap();
        db.remove_file("test.php").unwrap();
        assert!(db.find_symbols("Foo", None).unwrap().is_empty());
        assert!(db.find_references("Bar", None).unwrap().is_empty());
    }

    #[test]
    fn get_file_hash() {
        let db = Database::open_in_memory().unwrap();
        assert!(db.get_file_hash("missing.php").unwrap().is_none());
        db.upsert_file(&make_file("test.php")).unwrap();
        assert_eq!(
            db.get_file_hash("test.php").unwrap().as_deref(),
            Some("abc123")
        );
    }

    #[test]
    fn all_file_paths_sorted() {
        let db = Database::open_in_memory().unwrap();
        db.upsert_file(&make_file("z.php")).unwrap();
        db.upsert_file(&make_file("a.php")).unwrap();
        db.upsert_file(&make_file("m.php")).unwrap();
        let paths = db.all_file_paths().unwrap();
        assert_eq!(paths, vec!["a.php", "m.php", "z.php"]);
    }

    #[test]
    fn kind_filter() {
        let db = Database::open_in_memory().unwrap();
        let file_id = db.upsert_file(&make_file("test.php")).unwrap();
        db.insert_symbols(
            file_id,
            &[
                make_symbol("foo", SymbolKind::Function),
                make_symbol("foo", SymbolKind::Method),
            ],
        )
        .unwrap();
        let all = db.find_symbols("foo", None).unwrap();
        assert_eq!(all.len(), 2);
        let funcs = db.find_symbols("foo", Some(SymbolKind::Function)).unwrap();
        assert_eq!(funcs.len(), 1);
    }

    #[test]
    fn counts() {
        let db = Database::open_in_memory().unwrap();
        let file_id = db.upsert_file(&make_file("test.php")).unwrap();
        db.insert_symbols(file_id, &[make_symbol("A", SymbolKind::Class)])
            .unwrap();
        db.insert_references(file_id, &[make_reference("B", ReferenceKind::Call)])
            .unwrap();
        assert_eq!(db.file_count().unwrap(), 1);
        assert_eq!(db.symbol_count().unwrap(), 1);
        assert_eq!(db.reference_count().unwrap(), 1);
    }

    fn make_embedding(seed: f32) -> Vec<f32> {
        let mut v = vec![0.0f32; 384];
        v[0] = seed;
        v[1] = 1.0 - seed;
        let norm = (v[0] * v[0] + v[1] * v[1]).sqrt();
        v[0] /= norm;
        v[1] /= norm;
        v
    }

    #[test]
    fn insert_and_count_chunks() {
        let db = Database::open_in_memory().unwrap();
        let chunks = vec![
            ("fn main() {}".to_string(), 1u32, 1u32, make_embedding(0.1)),
            ("fn helper() {}".to_string(), 3, 5, make_embedding(0.9)),
        ];
        db.insert_chunks("test.rs", "rust", &chunks).unwrap();
        assert_eq!(db.chunk_count().unwrap(), 2);
    }

    #[test]
    fn delete_chunks_for_file() {
        let db = Database::open_in_memory().unwrap();
        db.insert_chunks(
            "a.rs",
            "rust",
            &[("code a".to_string(), 1, 5, make_embedding(0.1))],
        )
        .unwrap();
        db.insert_chunks(
            "b.rs",
            "rust",
            &[("code b".to_string(), 1, 5, make_embedding(0.9))],
        )
        .unwrap();
        assert_eq!(db.chunk_count().unwrap(), 2);
        db.delete_chunks_for_file("a.rs").unwrap();
        assert_eq!(db.chunk_count().unwrap(), 1);
        let paths = db.all_chunk_file_paths().unwrap();
        assert_eq!(paths, vec!["b.rs"]);
    }

    #[test]
    fn knn_search_returns_results() {
        let db = Database::open_in_memory().unwrap();
        let e_close = make_embedding(0.1);
        let e_far = make_embedding(0.9);
        db.insert_chunks(
            "close.rs",
            "rust",
            &[("close code".to_string(), 1, 5, e_close.clone())],
        )
        .unwrap();
        db.insert_chunks("far.rs", "rust", &[("far code".to_string(), 1, 5, e_far)])
            .unwrap();
        let query_bytes = super::embedding_to_bytes(&e_close);
        let results = db.search_knn(&query_bytes, 2, None).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].file_path, "close.rs");
    }

    #[test]
    fn chunks_for_file_returns_file_chunks_ordered() {
        let db = Database::open_in_memory().unwrap();
        db.insert_chunks(
            "a.rs",
            "rust",
            &[
                ("third chunk".to_string(), 30, 40, make_embedding(0.3)),
                ("first chunk".to_string(), 1, 10, make_embedding(0.1)),
                ("second chunk".to_string(), 15, 25, make_embedding(0.2)),
            ],
        )
        .unwrap();
        db.insert_chunks(
            "b.rs",
            "rust",
            &[("other file".to_string(), 1, 5, make_embedding(0.5))],
        )
        .unwrap();

        let chunks = db.chunks_for_file("a.rs").unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[1].start_line, 15);
        assert_eq!(chunks[2].start_line, 30);

        let empty = db.chunks_for_file("missing.rs").unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn knn_search_language_filter() {
        let db = Database::open_in_memory().unwrap();
        db.insert_chunks(
            "a.rs",
            "rust",
            &[("rust code".to_string(), 1, 5, make_embedding(0.1))],
        )
        .unwrap();
        db.insert_chunks(
            "b.py",
            "python",
            &[("python code".to_string(), 1, 5, make_embedding(0.2))],
        )
        .unwrap();
        let query_bytes = super::embedding_to_bytes(&make_embedding(0.15));
        let results = db.search_knn(&query_bytes, 10, Some("python")).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].language, "python");
    }
}
