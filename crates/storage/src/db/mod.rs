//! `Database` connection wrapper + split impls.
//!
//! Public API is stable: `Database`, `RawSearchRow`, `GitFileRow`, `CoChangeRow`,
//! and `embedding_to_bytes` are re-exported from this module so existing callers
//! (`use codesage_storage::{Database, RawSearchRow, embedding_to_bytes}`) keep
//! working. The methods themselves live in one of three `impl Database` blocks:
//!
//! - `structural` — files / symbols / refs / dependencies
//! - `semantic` — chunk table + sqlite-vec KNN + fullscan
//! - `git_hist` — git_files / git_co_changes / git_index_state
//!
//! Each `.rs` file owns a focused concern. Helpers that are truly shared across
//! blocks (row-kind parsers, embedding bytes) stay here.

use std::path::Path;

use anyhow::Result;
use codesage_protocol::{ReferenceKind, SymbolKind};
use rusqlite::{Connection, OptionalExtension, params};

use crate::schema::{
    ensure_chunk_table, fts_table_name, init_db, init_vec_extension, model_table_name,
    model_table_prefix,
};

pub use codesage_protocol::DEFAULT_EMBEDDING_DIM;

mod features;
mod git_hist;
mod semantic;
mod structural;

pub use git_hist::{CoChangeRow, GitFileRow};
pub use semantic::{RawSearchRow, SemanticFreshness, embedding_to_bytes};

/// Decode a stored DB string column into an enum via its `parse`, surfacing an
/// unknown value as a typed rusqlite error rather than silently relabeling it.
/// Loud failure is the right default: an unknown value almost always means
/// schema/binary skew. `label` names the enum in the error.
pub(super) fn row_enum<T>(
    s: &str,
    parse: impl Fn(&str) -> Option<T>,
    label: &str,
) -> rusqlite::Result<T> {
    parse(s).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("unknown {label} in row: {s:?}").into(),
        )
    })
}

pub(super) fn row_symbol_kind(s: &str) -> rusqlite::Result<SymbolKind> {
    row_enum(s, SymbolKind::parse, "SymbolKind")
}

pub(super) fn row_reference_kind(s: &str) -> rusqlite::Result<ReferenceKind> {
    row_enum(s, ReferenceKind::parse, "ReferenceKind")
}

/// Read a single-row (`id = 1`) index-state table's `(last_sha,
/// last_indexed_at)`, treating a missing row or an empty SHA as "no state".
/// Shared by the structural and git-history index-state accessors, which differ
/// only in the table name. `table` is a hardcoded constant at every call site,
/// never user input, so interpolation is safe.
pub(super) fn get_index_state(conn: &Connection, table: &str) -> Result<Option<(String, i64)>> {
    let sql = format!("SELECT last_sha, last_indexed_at FROM {table} WHERE id = 1");
    let row = conn.query_row(&sql, [], |r| {
        Ok((r.get::<_, Option<String>>(0)?, r.get::<_, i64>(1)?))
    });
    match row {
        Ok((Some(sha), at)) if !sha.is_empty() => Ok(Some((sha, at))),
        Ok(_) => Ok(None),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Stamp `(id = 1, last_sha, last_indexed_at = unixepoch())` on a single-row
/// index-state table. See [`get_index_state`] for the table-name contract.
pub(super) fn set_index_state(conn: &Connection, table: &str, sha: &str) -> Result<()> {
    let sql = format!(
        "INSERT INTO {table} (id, last_sha, last_indexed_at)
         VALUES (1, ?1, unixepoch())
         ON CONFLICT(id) DO UPDATE SET
             last_sha = excluded.last_sha,
             last_indexed_at = excluded.last_indexed_at"
    );
    conn.execute(&sql, params![sha])?;
    Ok(())
}

pub struct Database {
    pub(super) conn: Connection,
    pub(super) chunk_table: String,
}

fn quote_ident(identifier: &str) -> String {
    identifier.replace('"', "\"\"")
}

fn existing_chunk_table_name(conn: &Connection, chunk_table: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT m.name FROM sqlite_master m
         WHERE m.type = 'table' AND lower(m.name) = lower(?1)
           AND EXISTS (
               SELECT 1 FROM sqlite_master aux
               WHERE aux.type = 'table' AND lower(aux.name) = lower(?1 || '_info')
           )",
            params![chunk_table],
            |row| row.get(0),
        )
        .optional()?)
}

fn chunk_table_exists(conn: &Connection, chunk_table: &str) -> Result<bool> {
    Ok(existing_chunk_table_name(conn, chunk_table)?.is_some())
}

fn chunk_table_count(conn: &Connection, chunk_table: &str) -> Result<i64> {
    let sql = format!("SELECT COUNT(*) FROM \"{}\"", quote_ident(chunk_table));
    Ok(conn.query_row(&sql, [], |row| row.get(0))?)
}

fn semantic_model_metadata(conn: &Connection, chunk_table: &str) -> Result<Option<(String, i64)>> {
    Ok(conn
        .query_row(
            "SELECT model, dim FROM semantic_models
             WHERE lower(chunk_table) = lower(?1)
             ORDER BY CASE WHEN chunk_table = ?1 THEN 0 ELSE 1 END, chunk_table
             LIMIT 1",
            params![chunk_table],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?)
}

fn legacy_chunk_table_candidates(conn: &Connection, model: &str) -> Result<Vec<String>> {
    let prefix = model_table_prefix(model);
    let mut stmt = conn.prepare(
        "SELECT m.name FROM sqlite_master m
         LEFT JOIN semantic_models sm ON lower(sm.chunk_table) = lower(m.name)
         WHERE m.type = 'table'
           AND lower(m.name) GLOB lower(?1)
           AND sm.chunk_table IS NULL
           AND EXISTS (
               SELECT 1 FROM sqlite_master aux
               WHERE aux.type = 'table' AND aux.name = m.name || '_info'
           )
         ORDER BY m.name",
    )?;
    let rows = stmt
        .query_map(params![format!("{prefix}*")], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut populated = Vec::new();
    for table in rows {
        let lower_table = table.to_lowercase();
        let lower_prefix = prefix.to_lowercase();
        let Some(dim_suffix) = lower_table.strip_prefix(&lower_prefix) else {
            continue;
        };
        if !dim_suffix.is_empty()
            && dim_suffix.bytes().all(|b| b.is_ascii_digit())
            && chunk_table_count(conn, &table)? > 0
        {
            populated.push(table);
        }
    }
    Ok(populated)
}

pub(super) fn drop_chunk_table_group(conn: &Connection, table_name: &str) -> Result<()> {
    if !table_name.starts_with("chunks_") {
        anyhow::bail!("refusing to drop non-chunks table: {table_name}");
    }
    let fts_table = fts_table_name(table_name);
    let vocab_table = format!("{fts_table}_vocab");
    let vocab_sql = format!("DROP TABLE IF EXISTS \"{}\"", quote_ident(&vocab_table));
    conn.execute(&vocab_sql, [])?;
    let fts_sql = format!("DROP TABLE IF EXISTS \"{}\"", quote_ident(&fts_table));
    conn.execute(&fts_sql, [])?;
    conn.execute(
        "DELETE FROM semantic_files WHERE chunk_table = ?1",
        params![table_name],
    )?;
    conn.execute(
        "DELETE FROM semantic_models WHERE chunk_table = ?1",
        params![table_name],
    )?;
    let sql = format!("DROP TABLE IF EXISTS \"{}\"", quote_ident(table_name));
    conn.execute(&sql, [])?;
    Ok(())
}

fn ensure_semantic_model_compatible(
    conn: &Connection,
    chunk_table: &str,
    model: &str,
    dim: usize,
) -> Result<()> {
    let existing = semantic_model_metadata(conn, chunk_table)?;

    if let Some((existing_model, existing_dim)) = existing.as_ref()
        && (existing_model != model || *existing_dim != dim as i64)
    {
        anyhow::bail!(
            "chunk table {chunk_table:?} is already recorded for model {existing_model:?} dim {existing_dim}, cannot reuse for model {model:?} dim {dim}"
        );
    }

    if existing.is_none() && chunk_table_exists(conn, chunk_table)? {
        let rows = chunk_table_count(conn, chunk_table)?;
        if rows > 0 {
            anyhow::bail!(
                "chunk table {chunk_table:?} exists with {rows} chunks but has no exact semantic model metadata; run `codesage index --full` with this CodeSage version to rebuild it before using model {model:?} dim {dim}"
            );
        }
    }

    Ok(())
}

fn record_semantic_model_table(
    conn: &Connection,
    chunk_table: &str,
    model: &str,
    dim: usize,
) -> Result<()> {
    conn.execute(
        "DELETE FROM semantic_models
         WHERE lower(chunk_table) = lower(?1) AND chunk_table <> ?1",
        params![chunk_table],
    )?;
    conn.execute(
        "INSERT INTO semantic_models (chunk_table, model, dim, indexed_at)
         VALUES (?1, ?2, ?3, unixepoch())
         ON CONFLICT(chunk_table) DO UPDATE SET
             model = excluded.model,
             dim = excluded.dim,
             indexed_at = excluded.indexed_at",
        params![chunk_table, model, dim as i64],
    )?;
    Ok(())
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
        let requested_table = model_table_name(model, dim);
        let chunk_table =
            existing_chunk_table_name(&conn, &requested_table)?.unwrap_or(requested_table);
        ensure_semantic_model_compatible(&conn, &chunk_table, model, dim)?;
        ensure_chunk_table(&conn, &chunk_table, dim)?;
        record_semantic_model_table(&conn, &chunk_table, model, dim)?;
        Ok(Database { conn, chunk_table })
    }

    pub fn open_for_model_rebuild(path: &Path, model: &str, dim: usize) -> Result<Self> {
        init_vec_extension();
        let conn = Connection::open(path)?;
        init_db(&conn)?;
        let requested_table = model_table_name(model, dim);
        let chunk_table =
            existing_chunk_table_name(&conn, &requested_table)?.unwrap_or(requested_table);
        if chunk_table_exists(&conn, &chunk_table)? {
            match semantic_model_metadata(&conn, &chunk_table)? {
                Some((existing_model, existing_dim))
                    if existing_model == model && existing_dim == dim as i64 => {}
                _ => drop_chunk_table_group(&conn, &chunk_table)?,
            }
        }
        ensure_chunk_table(&conn, &chunk_table, dim)?;
        record_semantic_model_table(&conn, &chunk_table, model, dim)?;
        Ok(Database { conn, chunk_table })
    }

    /// Open a DB for structural queries plus best-effort chunk reads for an
    /// already-indexed model. This selects a single existing vec0 table by
    /// configured model name without constructing the embedder just to discover
    /// dimension. If no matching table exists, chunk reads degrade to empty
    /// results; if multiple tables match, the caller must resolve the ambiguity.
    pub fn open_for_existing_model(path: &Path, model: &str) -> Result<Self> {
        init_vec_extension();
        let conn = Connection::open(path)?;
        init_db(&conn)?;
        let matches = {
            let mut stmt = conn.prepare(
                "SELECT sm.chunk_table FROM semantic_models sm
                 JOIN sqlite_master m
                   ON m.type = 'table' AND lower(m.name) = lower(sm.chunk_table)
                 WHERE sm.model = ?1
                   AND EXISTS (
                       SELECT 1 FROM sqlite_master aux
                       WHERE aux.type = 'table'
                         AND lower(aux.name) = lower(sm.chunk_table || '_info')
                   )
                 ORDER BY sm.dim, sm.chunk_table",
            )?;
            stmt.query_map(params![model], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let chunk_table = match matches.as_slice() {
            [] => {
                let legacy = legacy_chunk_table_candidates(&conn, model)?;
                if !legacy.is_empty() {
                    anyhow::bail!(
                        "chunk table(s) match model {model:?} but lack exact semantic model metadata: {}; run `codesage index --full` with this CodeSage version to rebuild them",
                        legacy.join(", ")
                    );
                }
                String::new()
            }
            [table] => table.clone(),
            tables => {
                anyhow::bail!(
                    "multiple chunk tables recorded for model {model:?}: {}",
                    tables.join(", ")
                );
            }
        };
        Ok(Database { conn, chunk_table })
    }

    pub fn open_in_memory() -> Result<Self> {
        init_vec_extension();
        let conn = Connection::open_in_memory()?;
        init_db(&conn)?;
        let chunk_table = model_table_name("default", DEFAULT_EMBEDDING_DIM);
        ensure_semantic_model_compatible(&conn, &chunk_table, "default", DEFAULT_EMBEDDING_DIM)?;
        ensure_chunk_table(&conn, &chunk_table, DEFAULT_EMBEDDING_DIM)?;
        record_semantic_model_table(&conn, &chunk_table, "default", DEFAULT_EMBEDDING_DIM)?;
        Ok(Database { conn, chunk_table })
    }

    pub fn chunk_table_name(&self) -> &str {
        &self.chunk_table
    }

    pub fn execute_batch(&self, f: impl FnOnce(&Self) -> Result<()>) -> Result<()> {
        self.conn.execute_batch("BEGIN")?;
        match f(self) {
            Ok(()) => match self.conn.execute_batch("COMMIT") {
                Ok(()) => Ok(()),
                // SQLITE_BUSY or an I/O error during COMMIT can leave the
                // connection inside an open transaction; without an explicit
                // rollback every subsequent statement on this connection
                // fails with "cannot start a transaction within a
                // transaction" and poisons the long-lived MCP connection.
                Err(commit_err) => {
                    let _ = self.conn.execute_batch("ROLLBACK");
                    Err(commit_err.into())
                }
            },
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codesage_protocol::{FileInfo, Language, Reference, Symbol};

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
            rationale: vec![],
        }
    }

    fn make_qualified_symbol(name: &str, qualified_name: &str, kind: SymbolKind) -> Symbol {
        Symbol {
            name: name.to_string(),
            qualified_name: qualified_name.to_string(),
            kind,
            file_path: "test.php".to_string(),
            line_start: 1,
            line_end: 5,
            col_start: 0,
            col_end: 0,
            rationale: vec![],
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
    fn qualified_symbol_lookup_treats_scope_resolution_as_qualified() {
        let db = Database::open_in_memory().unwrap();
        let file_id = db.upsert_file(&make_file("db.rs")).unwrap();
        db.insert_symbols(
            file_id,
            &[make_qualified_symbol(
                "open",
                "Database::open",
                SymbolKind::Method,
            )],
        )
        .unwrap();

        let found = db.find_symbols("Database::open", None).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].qualified_name, "Database::open");
        assert!(db.symbol_exists("Database::open").unwrap());
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
    fn references_match_dotted_tail() {
        let db = Database::open_in_memory().unwrap();
        let file_id = db.upsert_file(&make_file("test.go")).unwrap();
        db.insert_references(
            file_id,
            &[make_reference("fmt.Println", ReferenceKind::Call)],
        )
        .unwrap();

        let found = db
            .find_references("Println", Some(ReferenceKind::Call))
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].to_name, "fmt.Println");
    }

    #[test]
    fn references_in_file_range_returns_only_refs_inside_lines() {
        let db = Database::open_in_memory().unwrap();
        let file_id = db.upsert_file(&make_file("test.php")).unwrap();
        db.insert_references(
            file_id,
            &[
                Reference {
                    line: 1,
                    col: 0,
                    ..make_reference("Before", ReferenceKind::Call)
                },
                Reference {
                    line: 3,
                    col: 0,
                    ..make_reference("Inside", ReferenceKind::Call)
                },
                Reference {
                    line: 8,
                    col: 0,
                    ..make_reference("After", ReferenceKind::Call)
                },
            ],
        )
        .unwrap();

        let found = db.references_in_file_range("test.php", 2, 5).unwrap();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].to_name, "Inside");
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

    fn create_legacy_chunk_table(path: &Path, model: &str, dim: usize, with_chunk: bool) -> String {
        init_vec_extension();
        let conn = Connection::open(path).unwrap();
        init_db(&conn).unwrap();
        let table = model_table_name(model, dim);
        ensure_chunk_table(&conn, &table, dim).unwrap();
        let db = Database {
            conn,
            chunk_table: table.clone(),
        };
        if with_chunk {
            let embedding = make_embedding(0.1);
            db.insert_chunks(
                "src/lib.rs",
                "rust",
                &[("fn target() {}", 1, 1, embedding.as_slice())],
            )
            .unwrap();
        }
        table
    }

    #[test]
    fn insert_and_count_chunks() {
        let db = Database::open_in_memory().unwrap();
        let e1 = make_embedding(0.1);
        let e2 = make_embedding(0.9);
        let chunks: Vec<(&str, u32, u32, &[f32])> = vec![
            ("fn main() {}", 1u32, 1u32, e1.as_slice()),
            ("fn helper() {}", 3, 5, e2.as_slice()),
        ];
        db.insert_chunks("test.rs", "rust", &chunks).unwrap();
        assert_eq!(db.chunk_count().unwrap(), 2);
    }

    #[test]
    fn delete_chunks_for_file() {
        let db = Database::open_in_memory().unwrap();
        let e_a = make_embedding(0.1);
        let e_b = make_embedding(0.9);
        db.insert_chunks("a.rs", "rust", &[("code a", 1, 5, e_a.as_slice())])
            .unwrap();
        db.insert_chunks("b.rs", "rust", &[("code b", 1, 5, e_b.as_slice())])
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
            &[("close code", 1, 5, e_close.as_slice())],
        )
        .unwrap();
        db.insert_chunks("far.rs", "rust", &[("far code", 1, 5, e_far.as_slice())])
            .unwrap();
        let query_bytes = embedding_to_bytes(&e_close);
        let results = db.search_knn(&query_bytes, 2, None).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].file_path, "close.rs");
    }

    #[test]
    fn chunks_for_file_returns_file_chunks_ordered() {
        let db = Database::open_in_memory().unwrap();
        let e1 = make_embedding(0.3);
        let e2 = make_embedding(0.1);
        let e3 = make_embedding(0.2);
        let e4 = make_embedding(0.5);
        db.insert_chunks(
            "a.rs",
            "rust",
            &[
                ("third chunk", 30, 40, e1.as_slice()),
                ("first chunk", 1, 10, e2.as_slice()),
                ("second chunk", 15, 25, e3.as_slice()),
            ],
        )
        .unwrap();
        db.insert_chunks("b.rs", "rust", &[("other file", 1, 5, e4.as_slice())])
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
        let e1 = make_embedding(0.1);
        let e2 = make_embedding(0.2);
        db.insert_chunks("a.rs", "rust", &[("rust code", 1, 5, e1.as_slice())])
            .unwrap();
        db.insert_chunks("b.py", "python", &[("python code", 1, 5, e2.as_slice())])
            .unwrap();
        let query_bytes = embedding_to_bytes(&make_embedding(0.15));
        let results = db.search_knn(&query_bytes, 10, Some("python")).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].language, "python");
    }

    #[test]
    fn structural_index_state_roundtrip() {
        let db = Database::open_in_memory().unwrap();
        assert!(db.get_structural_index_state().unwrap().is_none());

        db.set_structural_index_state("abc123").unwrap();
        let (sha, at) = db.get_structural_index_state().unwrap().unwrap();
        assert_eq!(sha, "abc123");
        assert!(at > 0, "last_indexed_at should be stamped with unixepoch");

        // Second stamp replaces rather than stacking (single-row table, id=1).
        db.set_structural_index_state("def456").unwrap();
        let (sha2, _) = db.get_structural_index_state().unwrap().unwrap();
        assert_eq!(sha2, "def456");
    }

    #[test]
    fn total_chunk_count_zero_on_fresh_db_open() {
        // A DB opened via `Database::open` has no chunk_table selected. Before
        // the total_chunk_count helper landed, `codesage status` would error
        // with "no such table: " because `chunk_count()` interpolated the
        // empty chunk_table. The replacement must tolerate the no-model case.
        use std::path::PathBuf;
        let tmp = std::env::temp_dir().join(format!(
            "codesage-total-chunk-test-{}.db",
            std::process::id()
        ));
        let tmp = PathBuf::from(&tmp);
        let _ = std::fs::remove_file(&tmp);
        // Materialize a DB with schema but no chunk tables. Using open() (no
        // model) is the code path exercised by `codesage status`.
        {
            let db = Database::open(&tmp).unwrap();
            assert_eq!(db.total_chunk_count().unwrap(), 0);
        }
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn list_vec_tables_excludes_fts_sidecars() {
        let db = Database::open_in_memory().unwrap();
        let tables = db.list_vec_tables().unwrap();
        assert_eq!(tables, vec![db.chunk_table_name().to_string()]);
    }

    #[test]
    fn drop_vec_table_removes_matching_fts_sidecar() {
        let db = Database::open_in_memory().unwrap();
        let table = db.chunk_table_name().to_string();
        let fts = crate::schema::fts_table_name(&table);
        let vocab = format!("{fts}_vocab");
        let embedding = make_embedding(0.1);
        db.insert_chunks(
            "src/lib.rs",
            "rust",
            &[("fn target() {}", 1, 1, embedding.as_slice())],
        )
        .unwrap();
        db.token_doc_frequency("target").unwrap();

        let fts_before: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?1",
                rusqlite::params![fts],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fts_before, 1);
        let vocab_before: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?1",
                rusqlite::params![vocab],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(vocab_before, 1);

        db.drop_vec_table(&table).unwrap();

        assert!(db.list_vec_tables().unwrap().is_empty());
        let fts_after: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?1",
                rusqlite::params![crate::schema::fts_table_name(&table)],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fts_after, 0);
        let vocab_after: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?1",
                rusqlite::params![format!("{}_vocab", crate::schema::fts_table_name(&table))],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(vocab_after, 0);
    }

    #[test]
    fn open_for_model_repairs_missing_fts_sidecar_from_vec_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let model = "codesage-test/model";
        let table;
        {
            let db = Database::open_for_model(&path, model, DEFAULT_EMBEDDING_DIM).unwrap();
            table = db.chunk_table_name().to_string();
            let embedding = make_embedding(0.1);
            db.insert_chunks(
                "src/lib.rs",
                "rust",
                &[(
                    "fn target() { let ColdFusion = true; }",
                    1,
                    1,
                    embedding.as_slice(),
                )],
            )
            .unwrap();
            let fts = crate::schema::fts_table_name(&table);
            let sql = format!("DROP TABLE \"{}\"", quote_ident(&fts));
            db.conn.execute(&sql, []).unwrap();
        }

        let db = Database::open_for_model(&path, model, DEFAULT_EMBEDDING_DIM).unwrap();
        let rows = db.search_bm25("\"ColdFusion\"", 10, None, None).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].file_path, "src/lib.rs");
    }

    #[test]
    fn semantic_file_hashes_roundtrip_and_delete() {
        let db = Database::open_in_memory().unwrap();
        assert!(db.all_semantic_file_hashes().unwrap().is_empty());

        db.upsert_semantic_file_hash("a.rs", "h1").unwrap();
        db.upsert_semantic_file_hash("b.rs", "h2").unwrap();
        db.upsert_semantic_file_hash("a.rs", "h3").unwrap();

        let hashes = db.all_semantic_file_hashes().unwrap();
        assert_eq!(hashes.get("a.rs").map(String::as_str), Some("h3"));
        assert_eq!(hashes.get("b.rs").map(String::as_str), Some("h2"));

        db.delete_semantic_file_hash("a.rs").unwrap();
        let hashes = db.all_semantic_file_hashes().unwrap();
        assert!(!hashes.contains_key("a.rs"));
        assert!(hashes.contains_key("b.rs"));
    }

    #[test]
    fn semantic_file_hashes_are_scoped_to_chunk_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");

        {
            let db = Database::open_for_model(&path, "model/a", DEFAULT_EMBEDDING_DIM).unwrap();
            db.upsert_semantic_file_hash("same.rs", "hash-a").unwrap();
        }
        {
            let db = Database::open_for_model(&path, "model/b", DEFAULT_EMBEDDING_DIM).unwrap();
            assert!(db.all_semantic_file_hashes().unwrap().is_empty());
            db.upsert_semantic_file_hash("same.rs", "hash-b").unwrap();
        }
        {
            let db = Database::open_for_model(&path, "model/a", DEFAULT_EMBEDDING_DIM).unwrap();
            let hashes = db.all_semantic_file_hashes().unwrap();
            assert_eq!(hashes.get("same.rs").map(String::as_str), Some("hash-a"));
        }
    }

    #[test]
    fn semantic_freshness_reports_missing_and_stale_files() {
        let db = Database::open_in_memory().unwrap();
        db.upsert_file(&FileInfo {
            path: "fresh.rs".to_string(),
            language: codesage_protocol::Language::Rust,
            content_hash: "fresh".to_string(),
        })
        .unwrap();
        db.upsert_file(&FileInfo {
            path: "stale.rs".to_string(),
            language: codesage_protocol::Language::Rust,
            content_hash: "new".to_string(),
        })
        .unwrap();
        db.upsert_file(&FileInfo {
            path: "missing.rs".to_string(),
            language: codesage_protocol::Language::Rust,
            content_hash: "missing".to_string(),
        })
        .unwrap();
        db.upsert_semantic_file_hash("fresh.rs", "fresh").unwrap();
        db.upsert_semantic_file_hash("stale.rs", "old").unwrap();

        let freshness = db
            .semantic_freshness()
            .unwrap()
            .expect("active chunk table");

        assert_eq!(freshness.indexed_files, 2);
        assert_eq!(freshness.missing_files, 1);
        assert_eq!(freshness.stale_files, 1);
        assert!(!freshness.is_fresh());
    }

    #[test]
    fn open_for_existing_model_selects_existing_chunk_table_without_creating_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let embedding = vec![0.0; DEFAULT_EMBEDDING_DIM];

        {
            let db = Database::open_for_model(&path, "codesage-test/model", DEFAULT_EMBEDDING_DIM)
                .unwrap();
            db.insert_chunks(
                "src/lib.rs",
                "rust",
                &[("fn target() {}", 1, 1, embedding.as_slice())],
            )
            .unwrap();
        }

        let db = Database::open_for_existing_model(&path, "codesage-test/model").unwrap();
        assert_eq!(db.chunks_for_file("src/lib.rs").unwrap().len(), 1);

        let db = Database::open_for_existing_model(&path, "codesage-test/missing").unwrap();
        assert!(db.chunks_for_file("src/lib.rs").unwrap().is_empty());
        assert_eq!(db.list_vec_tables().unwrap().len(), 1);
    }

    #[test]
    fn open_for_existing_model_requires_exact_model_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let indexed_model = "codesage-test/foo-bar";
        let colliding_model = "codesage-test/foo_bar";
        let table = model_table_name(indexed_model, DEFAULT_EMBEDDING_DIM);

        assert_eq!(
            table,
            model_table_name(colliding_model, DEFAULT_EMBEDDING_DIM),
            "test setup needs sanitized table-name collision"
        );

        Database::open_for_model(&path, indexed_model, DEFAULT_EMBEDDING_DIM).unwrap();

        let db = Database::open_for_existing_model(&path, indexed_model).unwrap();
        assert_eq!(db.chunk_table_name(), table.as_str());

        let db = Database::open_for_existing_model(&path, colliding_model).unwrap();
        assert_eq!(db.chunk_table_name(), "");
    }

    #[test]
    fn open_for_existing_model_ignores_legacy_prefix_collision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let requested_model = "codesage-test/foo";
        let legacy_model = "codesage-test/foo-bar";
        let legacy_table =
            create_legacy_chunk_table(&path, legacy_model, DEFAULT_EMBEDDING_DIM, true);

        assert!(legacy_table.starts_with(&model_table_prefix(requested_model)));

        let db = Database::open_for_existing_model(&path, requested_model).unwrap();

        assert_eq!(db.chunk_table_name(), "");
    }

    #[test]
    fn open_for_existing_model_rejects_legacy_chunk_table_without_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let model = "codesage-test/model";
        let table = create_legacy_chunk_table(&path, model, DEFAULT_EMBEDDING_DIM, true);

        let err = match Database::open_for_existing_model(&path, model) {
            Ok(_) => panic!("expected legacy metadata error"),
            Err(err) => err,
        };

        assert!(
            err.to_string()
                .contains("lack exact semantic model metadata"),
            "unexpected error: {err:#}"
        );
        assert!(
            err.to_string().contains(&table),
            "error should name legacy table {table}: {err:#}"
        );
    }

    #[test]
    fn open_for_model_rejects_populated_legacy_chunk_table_without_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let model = "codesage-test/model";
        create_legacy_chunk_table(&path, model, DEFAULT_EMBEDDING_DIM, true);

        let err = match Database::open_for_model(&path, model, DEFAULT_EMBEDDING_DIM) {
            Ok(_) => panic!("expected populated legacy table error"),
            Err(err) => err,
        };

        assert!(
            err.to_string()
                .contains("has no exact semantic model metadata"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn open_for_model_rebuild_replaces_populated_legacy_chunk_table_without_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let model = "codesage-test/model";
        let table = create_legacy_chunk_table(&path, model, DEFAULT_EMBEDDING_DIM, true);

        let db = Database::open_for_model_rebuild(&path, model, DEFAULT_EMBEDDING_DIM).unwrap();

        assert_eq!(db.chunk_table_name(), table.as_str());
        assert_eq!(db.chunk_count().unwrap(), 0);
        drop(db);

        let db = Database::open_for_existing_model(&path, model).unwrap();
        assert_eq!(db.chunk_table_name(), table.as_str());
    }

    #[test]
    fn open_for_model_rebuild_preserves_compatible_metadata_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let model = "codesage-test/model";
        let embedding = make_embedding(0.1);

        {
            let db = Database::open_for_model(&path, model, DEFAULT_EMBEDDING_DIM).unwrap();
            db.insert_chunks(
                "src/lib.rs",
                "rust",
                &[("fn target() {}", 1, 1, embedding.as_slice())],
            )
            .unwrap();
        }

        let db = Database::open_for_model_rebuild(&path, model, DEFAULT_EMBEDDING_DIM).unwrap();

        assert_eq!(db.chunk_count().unwrap(), 1);
    }

    #[test]
    fn open_for_model_rejects_sanitized_model_table_collision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let indexed_model = "codesage-test/foo-bar";
        let colliding_model = "codesage-test/foo_bar";

        Database::open_for_model(&path, indexed_model, DEFAULT_EMBEDDING_DIM).unwrap();
        let err = match Database::open_for_model(&path, colliding_model, DEFAULT_EMBEDDING_DIM) {
            Ok(_) => panic!("expected sanitized model table collision error"),
            Err(err) => err,
        };

        assert!(
            err.to_string().contains("already recorded for model"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn open_for_model_rejects_case_only_model_table_collision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let indexed_model = "codesage-test/Foo";
        let colliding_model = "codesage-test/foo";

        Database::open_for_model(&path, indexed_model, DEFAULT_EMBEDDING_DIM).unwrap();
        let err = match Database::open_for_model(&path, colliding_model, DEFAULT_EMBEDDING_DIM) {
            Ok(_) => panic!("expected case-only model table collision error"),
            Err(err) => err,
        };

        assert!(
            err.to_string().contains("already recorded for model"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn open_for_existing_model_rejects_ambiguous_chunk_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let model = "codesage-test/model";

        Database::open_for_model(&path, model, DEFAULT_EMBEDDING_DIM).unwrap();
        Database::open_for_model(&path, model, DEFAULT_EMBEDDING_DIM + 1).unwrap();

        let err = match Database::open_for_existing_model(&path, model) {
            Ok(_) => panic!("expected ambiguous chunk table error"),
            Err(err) => err,
        };

        assert!(
            err.to_string()
                .contains("multiple chunk tables recorded for model"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn drop_vec_table_removes_model_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let model = "codesage-test/model";

        {
            let db = Database::open_for_model(&path, model, DEFAULT_EMBEDDING_DIM).unwrap();
            let table = db.chunk_table_name().to_string();
            db.drop_vec_table(&table).unwrap();
        }

        let db = Database::open_for_existing_model(&path, model).unwrap();
        assert_eq!(db.chunk_table_name(), "");
    }

    #[test]
    fn structural_index_state_treats_empty_sha_as_absent() {
        let db = Database::open_in_memory().unwrap();
        // Direct INSERT rather than going through set_* so we can exercise the
        // empty-string branch of get_*.
        db.conn
            .execute(
                "INSERT INTO structural_index_state (id, last_sha, last_indexed_at)
                 VALUES (1, '', 0)",
                [],
            )
            .unwrap();
        assert!(db.get_structural_index_state().unwrap().is_none());
    }
}
