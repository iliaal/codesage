//! Files / symbols / refs / dependencies.

use anyhow::Result;
use codesage_protocol::{DependencyEntry, FileInfo, Reference, ReferenceKind, Symbol, SymbolKind};
use rusqlite::params;

use crate::schema::name_tail;

use super::{Database, row_reference_kind, row_symbol_kind};

impl Database {
    /// Return `(last_sha, last_indexed_at_unix)` for the structural index if a
    /// stamp exists. Mirrors [`Database::get_git_index_state`] but tracks the
    /// structural/semantic layer — not the git history layer. Used by drift
    /// instrumentation (see `codesage doctor`) to detect cases where git hooks
    /// failed to trigger a reindex.
    pub fn get_structural_index_state(&self) -> Result<Option<(String, i64)>> {
        let row = self.conn.query_row(
            "SELECT last_sha, last_indexed_at FROM structural_index_state WHERE id = 1",
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

    /// Stamp the HEAD SHA that the structural index was just built against.
    /// `indexed_at` is set to `unixepoch()` at the DB. Callers must only pass
    /// real SHAs — the "not a git repo" case is the caller's to skip.
    pub fn set_structural_index_state(&self, sha: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO structural_index_state (id, last_sha, last_indexed_at)
             VALUES (1, ?1, unixepoch())
             ON CONFLICT(id) DO UPDATE SET
                 last_sha = excluded.last_sha,
                 last_indexed_at = excluded.last_indexed_at",
            params![sha],
        )?;
        Ok(())
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
        let sql = if name.contains('\\') || name.contains('.') {
            "SELECT 1 FROM symbols WHERE qualified_name = ?1 LIMIT 1"
        } else {
            "SELECT 1 FROM symbols WHERE name = ?1 LIMIT 1"
        };
        let mut stmt = self.conn.prepare(sql)?;
        match stmt.query_row(params![name], |_| Ok(())) {
            Ok(()) => Ok(true),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
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
                kind: row_symbol_kind(&kind_str)?,
                file_path: row.get(3)?,
                line_start: row.get(4)?,
                line_end: row.get(5)?,
                col_start: row.get(6)?,
                col_end: row.get(7)?,
            })
        })?;

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

    fn query_refs(&self, sql: &str, params: impl rusqlite::Params) -> Result<Vec<Reference>> {
        let mut stmt = self.conn.prepare(sql)?;
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

    pub fn list_file_dependencies(&self, file_path: &str) -> Result<DependencyEntry> {
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
             WHERE r.to_name = ?1 AND r.kind IN ('import', 'include')",
        )?;
        let imported_by: Vec<String> = imported_by_stmt
            .query_map(params![file_path], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

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
                kind: row_symbol_kind(&kind_str)?,
                file_path: row.get(3)?,
                line_start: row.get(4)?,
                line_end: row.get(5)?,
                col_start: row.get(6)?,
                col_end: row.get(7)?,
            })
        })?;
        for sym_res in rows {
            let sym = sym_res?;
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
                    kind: row_symbol_kind(&kind_str)?,
                    file_path: row.get(3)?,
                    line_start: row.get(4)?,
                    line_end: row.get(5)?,
                    col_start: row.get(6)?,
                    col_end: row.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn remove_file(&self, path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM files WHERE path = ?1", params![path])?;
        // git tables are path-keyed (not FK'd to `files`, because git-index can run
        // without a structural index), so cascade manually. Without this, deleted
        // files stay visible in `find_coupling` / `assess_risk` / future hotspots.
        self.conn
            .execute("DELETE FROM git_files WHERE path = ?1", params![path])?;
        self.conn.execute(
            "DELETE FROM git_co_changes WHERE file_a = ?1 OR file_b = ?1",
            params![path],
        )?;
        Ok(())
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
}
