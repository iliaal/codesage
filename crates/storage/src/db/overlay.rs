use std::collections::HashSet;

use anyhow::{Context, Result, ensure};
use codesage_protocol::python::PythonBindings;
use codesage_protocol::{FileInfo, Language, Reference, Symbol, TrustBoundary};
use rusqlite::{OptionalExtension, params};

use super::{Database, FingerprintInput, RawSearchRow, row_enum};
use crate::schema::name_tail;

pub struct StructuralOverlayFile<'a> {
    pub file: &'a FileInfo,
    pub symbols: &'a [Symbol],
    pub references: &'a [Reference],
    pub python_bindings: &'a PythonBindings,
    pub fingerprints: &'a [FingerprintInput<'a>],
    pub trust_boundaries: &'a [TrustBoundary],
    pub interpretation: &'a str,
    pub source: &'a str,
}

const TABLES: &[(&str, &str)] = &[
    ("files", "id"),
    ("symbols", "file_id"),
    ("refs", "from_file_id"),
    ("python_bindings", "file_id"),
    ("symbol_fingerprints", "file_id"),
    ("file_trust_boundaries", "file_id"),
];

impl Database {
    pub fn get_indexed_file_hash(&self, path: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT content_hash FROM main.files WHERE path = ?1",
                [path],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn overlay_file_infos(&self) -> Result<Vec<FileInfo>> {
        let mut statement = self.conn.prepare(
            "SELECT path, language, content_hash, is_test FROM main.files ORDER BY path",
        )?;
        Ok(statement
            .query_map([], |row| {
                let language: String = row.get(1)?;
                Ok(FileInfo {
                    path: row.get(0)?,
                    language: row_enum(&language, Language::parse, "Language")?,
                    content_hash: row.get(2)?,
                    is_test: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn has_structural_overlay(&self) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_temp_schema WHERE name = '__overlay_dirty')",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn structural_overlay_paths(&self) -> Result<Vec<String>> {
        if !self.has_structural_overlay()? {
            return Ok(Vec::new());
        }
        let mut statement = self
            .conn
            .prepare("SELECT path FROM temp.__overlay_dirty ORDER BY path")?;
        Ok(statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Shadow structural reads on this read-only connection until it is dropped.
    /// Paths without replacement rows represent deletions. Hold a read snapshot
    /// across installation and the consuming query to keep clean rows consistent.
    /// Discard the connection if installation or rollback fails.
    pub fn install_structural_overlay(
        &self,
        dirty_paths: &[String],
        replacements: &[StructuralOverlayFile<'_>],
    ) -> Result<()> {
        codesage_protocol::work::checkpoint()?;
        ensure!(
            self.conn.is_readonly("main")?,
            "overlay requires a read-only index"
        );
        ensure!(!self.has_structural_overlay()?, "overlay already installed");
        let dirty: HashSet<&str> = dirty_paths.iter().map(String::as_str).collect();
        ensure!(dirty.len() == dirty_paths.len(), "duplicate overlay path");
        let mut replaced = HashSet::new();
        for replacement in replacements {
            ensure!(
                dirty.contains(replacement.file.path.as_str()),
                "replacement path is not dirty"
            );
            ensure!(
                replaced.insert(replacement.file.path.as_str()),
                "duplicate replacement path"
            );
        }
        self.conn.execute_batch("SAVEPOINT structural_overlay")?;
        let result = self
            .populate_structural_overlay(dirty_paths, replacements)
            .and_then(|()| {
                self.conn.execute_batch("RELEASE structural_overlay")?;
                Ok(())
            });
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                // A spent overlay deadline must not prevent undoing TEMP writes.
                let cleanup = codesage_protocol::work::WorkControl::new(None);
                let _cleanup = cleanup.enter();
                if let Err(rollback) = self
                    .conn
                    .execute_batch("ROLLBACK TO structural_overlay; RELEASE structural_overlay")
                {
                    return Err(error.context(format!(
                        "rolling back failed structural overlay: {rollback}; read snapshot active: {}",
                        self.read_snapshot_active()
                    )));
                }
                Err(error)
            }
        }
    }

    fn populate_structural_overlay(
        &self,
        dirty_paths: &[String],
        replacements: &[StructuralOverlayFile<'_>],
    ) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TEMP TABLE __overlay_dirty(path TEXT PRIMARY KEY, file_id INTEGER UNIQUE);
             CREATE TEMP TABLE __overlay_sources(path TEXT PRIMARY KEY, content TEXT NOT NULL,
                 language TEXT NOT NULL);",
        )?;
        for path in dirty_paths {
            self.conn.execute(
                "INSERT INTO temp.__overlay_dirty(path, file_id)
                 VALUES (?1, (SELECT id FROM main.files WHERE path = ?1))",
                [path],
            )?;
        }
        for (table, _) in TABLES {
            let rowid = if *table == "symbol_fingerprints" {
                "rowid AS rowid, "
            } else {
                ""
            };
            self.conn.execute_batch(&format!(
                "CREATE TEMP TABLE __overlay_{table} AS SELECT {rowid}* FROM main.{table} WHERE 0;"
            ))?;
        }
        self.conn.execute_batch(
            "CREATE UNIQUE INDEX temp.__overlay_files_id ON __overlay_files(id);
             CREATE UNIQUE INDEX temp.__overlay_files_path ON __overlay_files(path);
             CREATE UNIQUE INDEX temp.__overlay_symbols_id ON __overlay_symbols(id);
             CREATE INDEX temp.__overlay_symbols_name ON __overlay_symbols(name);
             CREATE INDEX temp.__overlay_symbols_qualified ON __overlay_symbols(qualified_name);
             CREATE INDEX temp.__overlay_symbols_file ON __overlay_symbols(file_id, qualified_name);
             CREATE UNIQUE INDEX temp.__overlay_refs_id ON __overlay_refs(id);
             CREATE INDEX temp.__overlay_refs_name ON __overlay_refs(to_name);
             CREATE INDEX temp.__overlay_refs_tail ON __overlay_refs(to_name_tail);
             CREATE INDEX temp.__overlay_refs_file ON __overlay_refs(from_file_id);
             CREATE UNIQUE INDEX temp.__overlay_python_key ON __overlay_python_bindings(file_id, kind, to_name, line, col);
             CREATE INDEX temp.__overlay_fingerprints_file ON __overlay_symbol_fingerprints(file_id);
             CREATE INDEX temp.__overlay_fingerprints_name ON __overlay_symbol_fingerprints(name);
             CREATE UNIQUE INDEX temp.__overlay_boundary_key ON __overlay_file_trust_boundaries(file_id, boundary);
             CREATE INDEX temp.__overlay_boundary ON __overlay_file_trust_boundaries(boundary);",
        )?;
        let mut file_id = self.overlay_max_id("files", "id")?;
        let mut symbol_id = self.overlay_max_id("symbols", "id")?;
        let mut reference_id = self.overlay_max_id("refs", "id")?;
        let mut fingerprint_id = self.overlay_max_id("symbol_fingerprints", "rowid")?;
        for replacement in replacements {
            codesage_protocol::work::checkpoint()?;
            let existing: Option<i64> = self
                .conn
                .query_row(
                    "SELECT id FROM main.files WHERE path = ?1",
                    [&replacement.file.path],
                    |row| row.get(0),
                )
                .optional()?;
            let id = match existing {
                Some(id) => id,
                None => next_id(&mut file_id)?,
            };
            self.conn.execute(
                "INSERT INTO temp.__overlay_files
                 (id, path, language, content_hash, indexed_at, boundaries_derived_at, interpretation, is_test)
                 VALUES (?1, ?2, ?3, ?4, unixepoch(), unixepoch(), ?5, ?6)",
                params![id, replacement.file.path, replacement.file.language.as_str(),
                    replacement.file.content_hash, replacement.interpretation, replacement.file.is_test],
            )?;
            self.conn.execute(
                "INSERT INTO temp.__overlay_sources(path, content, language) VALUES (?1, ?2, ?3)",
                params![
                    replacement.file.path,
                    replacement.source,
                    replacement.file.language.as_str()
                ],
            )?;
            let mut symbols = self.conn.prepare(
                "INSERT INTO temp.__overlay_symbols
                 (id, file_id, name, qualified_name, kind, line_start, line_end, col_start, col_end, rationale, visibility, is_test, signature, arity, parameters, return_type)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            )?;
            for symbol in replacement.symbols {
                codesage_protocol::work::checkpoint()?;
                ensure!(
                    symbol.file_path == replacement.file.path,
                    "overlay symbol has a different file"
                );
                symbols.execute(params![
                    next_id(&mut symbol_id)?,
                    id,
                    symbol.name,
                    symbol.qualified_name,
                    symbol.kind.as_str(),
                    symbol.line_start,
                    symbol.line_end,
                    symbol.col_start,
                    symbol.col_end,
                    serde_json::to_string(&symbol.rationale)?,
                    symbol.visibility.map(|value| value.as_str()),
                    symbol.is_test,
                    symbol
                        .signature
                        .as_ref()
                        .map(|signature| &signature.declaration),
                    symbol
                        .signature
                        .as_ref()
                        .and_then(|signature| signature.arity.as_ref())
                        .map(serde_json::to_string)
                        .transpose()?,
                    symbol
                        .signature
                        .as_ref()
                        .map(|signature| serde_json::to_string(&signature.parameters))
                        .transpose()?,
                    symbol
                        .signature
                        .as_ref()
                        .and_then(|signature| signature.returns.as_ref()),
                ])?;
            }
            let mut references = self.conn.prepare(
                "INSERT INTO temp.__overlay_refs
                 (id, from_file_id, from_symbol, to_name, to_name_tail, kind, line, col, lazy)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for reference in replacement.references {
                codesage_protocol::work::checkpoint()?;
                ensure!(
                    reference.from_file == replacement.file.path,
                    "overlay reference has a different file"
                );
                references.execute(params![
                    next_id(&mut reference_id)?,
                    id,
                    reference.from_symbol,
                    reference.to_name,
                    name_tail(&reference.to_name),
                    reference.kind.as_str(),
                    reference.line,
                    reference.col,
                    reference.lazy
                ])?;
            }
            let mut bindings = self.conn.prepare(
                "INSERT INTO temp.__overlay_python_bindings
                 (file_id, kind, to_name, line, col, target) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for binding in &replacement.python_bindings.references {
                codesage_protocol::work::checkpoint()?;
                bindings.execute(params![
                    id,
                    binding.kind.as_str(),
                    binding.to_name,
                    binding.line,
                    binding.col,
                    serde_json::to_string(&binding.target)?
                ])?;
            }
            for (name, target) in &replacement.python_bindings.exports {
                codesage_protocol::work::checkpoint()?;
                bindings.execute(params![
                    id,
                    "export",
                    name,
                    0,
                    0,
                    serde_json::to_string(target)?
                ])?;
            }
            for binding in &replacement.python_bindings.classes {
                codesage_protocol::work::checkpoint()?;
                bindings.execute(params![
                    id,
                    "class_member",
                    format!("{}.{}", binding.qualified_class, binding.name),
                    binding.definition_line,
                    0,
                    serde_json::to_string(&binding.target)?
                ])?;
            }
            for fingerprint in replacement.fingerprints {
                codesage_protocol::work::checkpoint()?;
                ensure!(
                    fingerprint.fp.len() == 64,
                    "invalid overlay fingerprint length"
                );
                let bytes: Vec<u8> = fingerprint
                    .fp
                    .iter()
                    .flat_map(|value| value.to_le_bytes())
                    .collect();
                self.conn.execute(
                    "INSERT INTO temp.__overlay_symbol_fingerprints
                     (rowid, file_id, name, kind, line_start, line_end, leaf_count, fp)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        next_id(&mut fingerprint_id)?,
                        id,
                        fingerprint.name,
                        fingerprint.kind,
                        fingerprint.line_start,
                        fingerprint.line_end,
                        fingerprint.leaf_count,
                        bytes
                    ],
                )?;
            }
            for boundary in replacement.trust_boundaries {
                self.conn.execute(
                    "INSERT OR IGNORE INTO temp.__overlay_file_trust_boundaries(file_id, boundary) VALUES (?1, ?2)",
                    params![id, boundary.as_str()],
                )?;
            }
        }
        for (table, key) in TABLES {
            let rowid = if *table == "symbol_fingerprints" {
                "rowid, "
            } else {
                ""
            };
            self.conn.execute_batch(&format!(
                "CREATE TEMP VIEW {table} AS
                 SELECT {rowid}* FROM main.{table} WHERE {key} NOT IN
                    (SELECT file_id FROM temp.__overlay_dirty WHERE file_id IS NOT NULL)
                 UNION ALL SELECT * FROM temp.__overlay_{table};"
            ))?;
        }
        codesage_protocol::work::checkpoint()?;
        Ok(())
    }

    fn overlay_max_id(&self, table: &str, key: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            &format!("SELECT COALESCE(MAX({key}), 0) FROM main.{table}"),
            [],
            |row| row.get(0),
        )?)
    }

    pub(super) fn overlay_source_chunks(&self, path: &str) -> Result<Option<Vec<RawSearchRow>>> {
        if !self.has_structural_overlay()? {
            return Ok(None);
        }
        let dirty: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM temp.__overlay_dirty WHERE path = ?1)",
            [path],
            |row| row.get(0),
        )?;
        if !dirty {
            return Ok(None);
        }
        let source: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT content, language FROM temp.__overlay_sources WHERE path = ?1",
                [path],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((content, language)) = source else {
            return Ok(Some(Vec::new()));
        };
        Ok(Some(source_chunks(path, &language, &content)))
    }

    pub fn overlay_source_chunk_at_line(
        &self,
        path: &str,
        line: u32,
    ) -> Result<Option<RawSearchRow>> {
        if line == 0 || !self.has_structural_overlay()? {
            return Ok(None);
        }
        let source: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT content, language FROM temp.__overlay_sources WHERE path = ?1",
                [path],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((content, language)) = source else {
            return Ok(None);
        };
        let lines: Vec<_> = content.split_inclusive('\n').collect();
        let requested = line as usize - 1;
        if requested >= lines.len() {
            return Ok(None);
        }
        let definition_start = self
            .symbols_for_file(path)?
            .into_iter()
            .filter(|symbol| symbol.line_start <= line && line <= symbol.line_end)
            .min_by_key(|symbol| symbol.line_end - symbol.line_start)
            .map_or(requested, |symbol| {
                symbol.line_start.saturating_sub(1) as usize
            });
        let prefix = &lines[definition_start..requested];
        let start =
            if prefix.len() < 32 && prefix.iter().map(|line| line.len()).sum::<usize>() < 512 {
                definition_start
            } else {
                requested
            };
        let mut end = start;
        let mut bytes = 0;
        while end < lines.len() && end - start < 64 {
            let size = lines[end].len();
            if end > requested && bytes + size > 2048 {
                break;
            }
            bytes += size;
            end += 1;
        }
        Ok(Some(RawSearchRow {
            retrieval: Default::default(),
            file_path: path.to_owned(),
            language,
            start_line: start as u32 + 1,
            end_line: end as u32,
            content: lines[start..end].concat(),
            distance: 0.0,
        }))
    }
}

fn source_chunks(path: &str, language: &str, content: &str) -> Vec<RawSearchRow> {
    const CHUNK_BYTES: usize = 2048;
    const CHUNK_LINES: u32 = 64;
    let mut chunks = Vec::new();
    let mut chunk_start = 0;
    let mut byte = 0;
    let mut start_line = 1;
    let mut last_line = 1;
    let mut push = |start: usize, end: usize, first: u32, last: u32| {
        chunks.push(RawSearchRow {
            retrieval: Default::default(),
            file_path: path.to_owned(),
            language: language.to_owned(),
            start_line: first,
            end_line: last,
            content: content[start..end].to_owned(),
            distance: 0.0,
        });
    };
    for (index, line) in content.split_inclusive('\n').enumerate() {
        let line_number = index as u32 + 1;
        if byte > chunk_start
            && (byte - chunk_start + line.len() > CHUNK_BYTES
                || line_number - start_line >= CHUNK_LINES)
        {
            push(chunk_start, byte, start_line, line_number - 1);
            chunk_start = byte;
            start_line = line_number;
        }
        byte += line.len();
        last_line = line_number;
    }
    if byte > chunk_start || content.is_empty() {
        push(chunk_start, byte, start_line, last_line);
    }
    chunks
}

fn next_id(id: &mut i64) -> Result<i64> {
    *id = id.checked_add(1).context("overlay row id exhausted")?;
    Ok(*id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codesage_protocol::python::PythonBindingTarget;
    use codesage_protocol::{ReferenceKind, SymbolKind};

    fn file(path: &str, hash: &str) -> FileInfo {
        FileInfo {
            path: path.into(),
            language: Language::Python,
            content_hash: hash.into(),
            is_test: false,
        }
    }

    fn symbol(path: &str, name: &str) -> Symbol {
        Symbol {
            name: name.into(),
            qualified_name: name.into(),
            kind: SymbolKind::Function,
            file_path: path.into(),
            line_start: 1,
            line_end: 2,
            col_start: 0,
            col_end: 8,
            rationale: Vec::new(),
            visibility: None,
            is_test: false,
            signature: None,
            overloaded: false,
        }
    }

    fn reference(path: &str, from: &str, to: &str) -> Reference {
        Reference {
            from_file: path.into(),
            from_symbol: Some(from.into()),
            to_name: to.into(),
            kind: ReferenceKind::Call,
            line: 2,
            col: 4,
            lazy: false,
            is_test: false,
            to: None,
            from_line: None,
        }
    }

    fn seed(path: &std::path::Path) {
        let db = Database::open(path).unwrap();
        for (path, name) in [
            ("dirty.py", "old"),
            ("clean.py", "clean"),
            ("deleted.py", "gone"),
        ] {
            let id = db.upsert_file(&file(path, "indexed")).unwrap();
            db.insert_symbols(id, &[symbol(path, name)]).unwrap();
            db.insert_references(id, &[reference(path, name, "old")])
                .unwrap();
            let mut bindings = PythonBindings::default();
            bindings.exports.push((
                name.into(),
                PythonBindingTarget {
                    name: name.into(),
                    ..Default::default()
                },
            ));
            db.insert_python_bindings(id, &bindings).unwrap();
            db.insert_fingerprints(
                id,
                &[FingerprintInput {
                    name,
                    kind: "function",
                    line_start: 1,
                    line_end: 2,
                    leaf_count: 3,
                    fp: &[1; 64],
                }],
            )
            .unwrap();
            db.replace_file_trust_boundaries(id, &[TrustBoundary::Filesystem])
                .unwrap();
        }
        db.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
    }

    #[test]
    fn overlay_replaces_all_structural_rows_without_changing_index() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("index.db");
        seed(&path);
        let bytes = std::fs::read(&path).unwrap();
        let db = Database::open_read_only(&path).unwrap();
        assert_eq!(
            db.conn
                .query_row("PRAGMA temp_store", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            2
        );
        let _snapshot = db.read_snapshot().unwrap();
        let original_id = db.file_id_for_path("dirty.py").unwrap();
        let info = file("dirty.py", "edited");
        let mut changed = symbol("dirty.py", "current");
        changed.is_test = true;
        let mut bindings = PythonBindings::default();
        bindings.exports.push((
            "current".into(),
            PythonBindingTarget {
                name: "current".into(),
                ..Default::default()
            },
        ));
        db.install_structural_overlay(
            &["dirty.py".into(), "deleted.py".into()],
            &[StructuralOverlayFile {
                file: &info,
                symbols: &[changed.clone()],
                references: &[reference("dirty.py", "current", "clean")],
                python_bindings: &bindings,
                fingerprints: &[FingerprintInput {
                    name: "current",
                    kind: "function",
                    line_start: 1,
                    line_end: 2,
                    leaf_count: 4,
                    fp: &[2; 64],
                }],
                trust_boundaries: &[TrustBoundary::Database],
                interpretation: "current",
                source: "def current():\n    clean()\n",
            }],
        )
        .unwrap();
        assert!(db.find_symbols("old", None).unwrap().is_empty());
        assert!(db.find_symbols("gone", None).unwrap().is_empty());
        assert_eq!(db.find_symbols("current", None).unwrap(), [changed]);
        assert_eq!(
            db.find_symbols("clean", None).unwrap(),
            [symbol("clean.py", "clean")]
        );
        assert_eq!(db.file_id_for_path("dirty.py").unwrap(), original_id);
        assert_eq!(
            db.get_file_hash("dirty.py").unwrap().as_deref(),
            Some("edited")
        );
        assert_eq!(
            db.get_indexed_file_hash("dirty.py").unwrap().as_deref(),
            Some("indexed")
        );
        assert_eq!(db.file_id_for_path("deleted.py").unwrap(), None);
        let refs = db.find_references("clean", None).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].from_file, "dirty.py");
        assert!(refs[0].is_test);
        let old_refs = db.find_references("old", None).unwrap();
        assert_eq!(old_refs.len(), 1);
        assert_eq!(old_refs[0].from_file, "clean.py");
        assert!(
            db.python_export_binding("dirty.py", "old")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db.python_export_binding("dirty.py", "current")
                .unwrap()
                .unwrap()
                .name,
            "current"
        );
        assert!(
            db.python_export_binding("clean.py", "clean")
                .unwrap()
                .is_some()
        );
        assert!(
            db.python_export_binding("deleted.py", "gone")
                .unwrap()
                .is_none()
        );
        assert_eq!(db.all_fingerprints().unwrap().len(), 2);
        assert_eq!(db.fingerprints_named("current").unwrap()[0].fp, vec![2; 64]);
        assert!(db.fingerprints_named("old").unwrap().is_empty());
        assert_eq!(db.fingerprint_validity_token().unwrap().0, 2);
        assert!(db.fingerprint_cache_key().is_none());
        assert_eq!(
            db.trust_boundaries_for_file_path("dirty.py").unwrap(),
            [TrustBoundary::Database]
        );
        assert_eq!(
            db.trust_boundaries_for_file_path("clean.py").unwrap(),
            [TrustBoundary::Filesystem]
        );
        assert!(
            db.trust_boundaries_for_file_path("deleted.py")
                .unwrap()
                .is_empty()
        );
        let chunks = db.chunks_for_file("dirty.py").unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].content, "def current():\n    clean()\n");
        assert_eq!(chunks[0].end_line, 2);
        assert!(db.chunks_for_file("deleted.py").unwrap().is_empty());
        assert!(db.conn.execute("DELETE FROM main.files", []).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let separate = Database::open_read_only(&path).unwrap();
        assert_eq!(separate.find_symbols("old", None).unwrap().len(), 1);
        assert_eq!(separate.find_symbols("gone", None).unwrap().len(), 1);
        assert!(separate.find_symbols("current", None).unwrap().is_empty());
        let plan: Vec<String> = db
            .conn
            .prepare("EXPLAIN QUERY PLAN SELECT name FROM symbols WHERE name = 'current'")
            .unwrap()
            .query_map([], |row| row.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            plan.iter().any(|line| line.contains("idx_symbols_name")),
            "{plan:?}"
        );
        assert!(
            plan.iter()
                .any(|line| line.contains("__overlay_symbols_name")),
            "{plan:?}"
        );
    }

    #[test]
    fn failed_overlay_rolls_back_rows_and_views_inside_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("index.db");
        seed(&path);
        let db = Database::open_read_only(&path).unwrap();
        let _snapshot = db.read_snapshot().unwrap();
        let info = file("dirty.py", "edited");
        let error = db
            .install_structural_overlay(
                &["dirty.py".into(), "deleted.py".into()],
                &[StructuralOverlayFile {
                    file: &info,
                    symbols: &[symbol("dirty.py", "current")],
                    references: &[],
                    python_bindings: &PythonBindings::default(),
                    fingerprints: &[FingerprintInput {
                        name: "current",
                        kind: "function",
                        line_start: 1,
                        line_end: 2,
                        leaf_count: 3,
                        fp: &[2],
                    }],
                    trust_boundaries: &[],
                    interpretation: "current",
                    source: "def current(): pass\n",
                }],
            )
            .unwrap_err();
        assert!(error.to_string().contains("invalid overlay fingerprint"));
        assert!(!db.has_structural_overlay().unwrap());
        assert_eq!(db.find_symbols("old", None).unwrap().len(), 1);
        assert_eq!(db.find_symbols("gone", None).unwrap().len(), 1);
        assert!(db.find_symbols("current", None).unwrap().is_empty());
        assert!(!db.conn.is_autocommit());
        assert_eq!(
            db.conn
                .query_row("SELECT count(*) FROM sqlite_temp_schema", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn sqlite_interruption_does_not_leave_partial_overlay() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("index.db");
        seed(&path);
        let db = Database::open_read_only(&path).unwrap();
        let _snapshot = db.read_snapshot().unwrap();
        let steps = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = steps.clone();
        db.conn
            .progress_handler(
                1,
                Some(move || count.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 800),
            )
            .unwrap();
        let error = db
            .install_structural_overlay(&["dirty.py".into()], &[])
            .unwrap_err();
        crate::db::install_work_control(&db.conn).unwrap();
        assert!(
            error.chain().any(|cause| matches!(
                cause.downcast_ref::<rusqlite::Error>(),
                Some(rusqlite::Error::SqliteFailure(error, _))
                    if error.code == rusqlite::ErrorCode::OperationInterrupted
            )),
            "{error:#}"
        );
        assert!(steps.load(std::sync::atomic::Ordering::Relaxed) > 800);
        assert!(!db.has_structural_overlay().unwrap());
        assert!(!db.read_snapshot_active());
        let writer = Database::open(&path).unwrap();
        writer
            .conn
            .execute(
                "UPDATE main.symbols SET name = 'changed' WHERE name = 'clean'",
                [],
            )
            .unwrap();
        assert_eq!(db.find_symbols("changed", None).unwrap().len(), 1);
        assert_eq!(db.find_symbols("old", None).unwrap().len(), 1);
        assert_eq!(
            db.conn
                .query_row("SELECT count(*) FROM sqlite_temp_schema", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn nested_snapshot_keeps_outer_snapshot_and_overlay_alive() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("index.db");
        seed(&path);
        let reader = Database::open_read_only(&path).unwrap();
        let outer = reader.read_snapshot().unwrap();
        reader
            .install_structural_overlay(&["dirty.py".into()], &[])
            .unwrap();
        let nested = reader.read_snapshot().unwrap();
        let writer = Database::open(&path).unwrap();
        writer
            .conn
            .execute(
                "UPDATE main.symbols SET name = 'changed' WHERE name = 'clean'",
                [],
            )
            .unwrap();
        drop(nested);
        assert!(!reader.conn.is_autocommit());
        assert!(reader.has_structural_overlay().unwrap());
        assert!(reader.find_symbols("old", None).unwrap().is_empty());
        assert_eq!(reader.find_symbols("clean", None).unwrap().len(), 1);
        assert!(reader.find_symbols("changed", None).unwrap().is_empty());
        drop(outer);
        assert!(reader.conn.is_autocommit());
        assert!(!reader.has_structural_overlay().unwrap());
        assert_eq!(reader.find_symbols("old", None).unwrap().len(), 1);
        assert_eq!(reader.find_symbols("changed", None).unwrap().len(), 1);
    }

    #[test]
    fn source_chunks_preserve_line_ranges_and_late_definition() {
        let source = format!(
            "{}def changed():\n    return 42\n",
            "# preceding commentary\n".repeat(1500)
        );
        let chunks = source_chunks("app/x.py", "python", &source);
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.content.as_str())
                .collect::<String>(),
            source
        );
        assert!(chunks.iter().all(|chunk| chunk.content.len() <= 2048));
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.end_line - chunk.start_line < 64)
        );
        for adjacent in chunks.windows(2) {
            assert_eq!(adjacent[0].end_line + 1, adjacent[1].start_line);
        }
        let last = chunks.last().unwrap();
        assert!(last.start_line <= 1501);
        assert_eq!(last.end_line, 1502);
        assert!(last.content.ends_with("def changed():\n    return 42\n"));
    }
}
