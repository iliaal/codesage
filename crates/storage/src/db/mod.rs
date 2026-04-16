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
use rusqlite::Connection;

use crate::schema::{ensure_chunk_table, init_db, init_vec_extension, model_table_name};

pub use codesage_protocol::DEFAULT_EMBEDDING_DIM;

mod git_hist;
mod semantic;
mod structural;

pub use git_hist::{CoChangeRow, GitFileRow};
pub use semantic::{RawSearchRow, embedding_to_bytes};

/// Parse a SymbolKind from a stored DB row, surfacing unknown variants as a typed
/// rusqlite error rather than silently relabeling them. Loud failure is the right
/// default here: an unknown kind almost always means schema/binary skew.
pub(super) fn row_symbol_kind(s: &str) -> rusqlite::Result<SymbolKind> {
    SymbolKind::parse(s).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("unknown SymbolKind in row: {s:?}").into(),
        )
    })
}

/// See [`row_symbol_kind`].
pub(super) fn row_reference_kind(s: &str) -> rusqlite::Result<ReferenceKind> {
    ReferenceKind::parse(s).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("unknown ReferenceKind in row: {s:?}").into(),
        )
    })
}

pub struct Database {
    pub(super) conn: Connection,
    pub(super) chunk_table: String,
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
}
