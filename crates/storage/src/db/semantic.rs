//! Chunk table + sqlite-vec KNN + fullscan search.

use std::collections::HashMap;

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use super::{Database, drop_chunk_table_group, quote_ident};
use crate::schema::{
    FtsSidecarHealth, fts_sidecar_health, path_sidecar_matches_auxiliary, path_table_name,
    sync_path_sidecar,
};

#[cfg(test)]
thread_local! {
    /// Runs once on this thread just before [`Database::confirmed_seed`]
    /// fixes a deferred transaction's snapshot, so a test can land a commit
    /// at that point.
    pub(super) static BEFORE_DEFERRED_SNAPSHOT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn run_before_deferred_snapshot_hook() {
    if let Some(hook) = BEFORE_DEFERRED_SNAPSHOT.with(|hook| hook.borrow_mut().take()) {
        hook();
    }
}

/// Per-handle sidecar verdicts, keyed by chunk table.
#[derive(Debug, Default)]
pub(super) struct SidecarMemo {
    pub(super) tables: HashMap<String, SidecarSync>,
    /// A sidecar rebuild ran inside a transaction that has not been seen to
    /// end. Until a transaction that has not written clears it, no verdict is
    /// remembered: a rollback would undo the rebuild that made the table look
    /// in sync.
    pub(super) uncommitted_rebuild: bool,
    /// Read-path verdicts on whether a chunk table's path sidecar names
    /// exactly its rows; see [`Database::path_sidecar_readable`].
    pub(super) readable_paths: HashMap<String, ReadVerdict>,
    /// Where committed-state read verdicts are offered beyond this handle;
    /// see [`super::PathSidecarVerdicts`].
    pub(super) verdict_sink: Option<super::sidecar_verdicts::Attachment>,
    /// A verdict another handle stored, trusted only inside a read snapshot
    /// its version is confirmed for; see [`Database::confirmed_seed`].
    pub(super) seed: Option<super::sidecar_verdicts::Seed>,
    /// Pair probes this handle has run; see
    /// [`Database::path_sidecar_probes_for_tests`].
    pub(super) probes: u32,
}

/// What one handle has established about a chunk table's sidecars.
#[derive(Debug, Clone, Copy)]
pub(super) struct SidecarSync {
    /// The FTS sidecar mirrors the chunk rowids, so deleting a file's FTS rows
    /// by those rowids removes all of them. When false (a diverged sidecar
    /// over the repair cap) FTS deletes keep the `file_path` scan, which also
    /// removes orphan rows naming the file.
    fts_by_rowid: bool,
    /// `PRAGMA data_version` on this connection for the snapshot the verdict
    /// was reached in; reuse requires the value unchanged.
    version: i64,
}

/// A read-path verdict and the `PRAGMA data_version` it was reached at. A
/// positive one is reused only while that value is unchanged; a negative one
/// stands, since the scan it selects is always correct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ReadVerdict {
    pub(super) readable: bool,
    pub(super) version: i64,
}

/// Opaque comparison token returned by
/// [`Database::semantic_files_validity_token`]. Components are
/// `(count, max_rowid, max_indexed_at, sum_path_len, sum_path_first_char)`;
/// callers only compare tokens for equality, never interpret the fields.
pub type SemanticValidityToken = (i64, i64, i64, i64, i64);

#[derive(Debug, Clone)]
pub struct RawSearchRow {
    pub file_path: String,
    pub language: String,
    pub content: String,
    pub start_line: u32,
    pub end_line: u32,
    pub distance: f32,
    /// Retrieval evidence retained when hybrid fusion replaces `distance`.
    pub retrieval: codesage_protocol::SearchScoreSignals,
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
    // sqlite-vec uses little-endian float32; unsupported targets must fail to build.
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

/// Inverse of [`embedding_to_bytes`]: decode a vec0 float32 blob. A blob whose
/// length is not a multiple of four is not a vector this crate wrote and is
/// refused rather than truncated.
pub fn embedding_from_bytes(bytes: &[u8]) -> Result<Vec<f32>> {
    anyhow::ensure!(
        bytes.len().is_multiple_of(4),
        "embedding blob of {} bytes is not a float32 vector",
        bytes.len()
    );
    Ok(bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect())
}

/// Shared search-column order; narrow SQLite REAL distances/BM25 scores from f64.
fn row_to_raw_search(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawSearchRow> {
    Ok(RawSearchRow {
        file_path: row.get(0)?,
        language: row.get(1)?,
        content: row.get(2)?,
        start_line: row.get(3)?,
        end_line: row.get(4)?,
        distance: row.get::<_, f64>(5)? as f32,
        retrieval: Default::default(),
    })
}

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

fn push_language_filter(
    conditions: &mut Vec<String>,
    param_values: &mut Vec<Box<dyn rusqlite::types::ToSql>>,
    languages: &[&str],
) {
    if languages.is_empty() {
        return;
    }
    let placeholders: Vec<String> = languages
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", param_values.len() + i + 1))
        .collect();
    conditions.push(format!("language IN ({})", placeholders.join(",")));
    for lang in languages {
        param_values.push(Box::new(lang.to_string()));
    }
}

/// Unprivileged free space (f_bavail × f_frsize), or None if unavailable.
/// The conversions accommodate u32 macOS fields and u64 Linux fields.
#[cfg(unix)]
#[allow(clippy::useless_conversion)]
fn available_disk_space(path: &str) -> Option<u64> {
    let c_path = std::ffi::CString::new(path).ok()?;
    // SAFETY: `c_path` is a valid NUL-terminated C string outliving the call,
    // and `st` is an out-parameter that statvfs fully initializes when it
    // returns 0; it is only read on that success path.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut st) };
    if rc != 0 {
        return None;
    }
    let blocks: u64 = st.f_bavail.into();
    let frsize: u64 = st.f_frsize.into();
    Some(blocks.saturating_mul(frsize))
}

#[cfg(not(unix))]
fn available_disk_space(_path: &str) -> Option<u64> {
    None
}

impl Database {
    fn fts_table(&self) -> String {
        crate::schema::fts_table_name(&self.chunk_table)
    }

    /// Bring `chunk_table`'s path sidecar in line with its `(id, path)` pairs
    /// before a write through it: a sidecar written by an older binary
    /// (missing, diverged by its inserts and deletes, or left over from a
    /// chunk table it dropped and repopulated) is rebuilt, so the rowid
    /// deletes below never leave a file's vectors behind or delete another
    /// file's. `fts_in_sync` carries an FTS verdict the caller already holds;
    /// `None` probes it.
    ///
    /// Runs inside the write's own transaction or savepoint, against the
    /// snapshot that transaction fixes. A remembered verdict, from this check
    /// or from [`Database::path_sidecar_readable`] (the same pair check), is
    /// reused only if `PRAGMA data_version` still reads the value it was
    /// reached at, so no other connection has committed since; this
    /// connection's own commits leave the value alone and keep both sides
    /// paired.
    ///
    /// A verdict is remembered unless a rebuild ran inside a still-open
    /// transaction. One reached inside a transaction without one holds after
    /// a rollback too: every chunk write through this handle runs this check
    /// first and keeps both sides equal, so the state the transaction started
    /// from was already in sync.
    pub(super) fn ensure_sidecars(
        &self,
        chunk_table: &str,
        fts_in_sync: Option<bool>,
    ) -> Result<SidecarSync> {
        anyhow::ensure!(
            !self.conn.is_autocommit(),
            "a chunk write checks its sidecars inside its own transaction"
        );
        if self.conn.transaction_state(Some("main"))? != rusqlite::TransactionState::Write {
            // A rebuild writes, so a transaction that has not written is not
            // the one that ran it: that one committed or rolled back.
            self.verified_sidecars.borrow_mut().uncommitted_rebuild = false;
        }
        let version = self.snapshot_data_version()?;
        let remembered = self
            .verified_sidecars
            .borrow()
            .tables
            .get(chunk_table)
            .copied();
        if let Some(sync) = remembered {
            if sync.version == version {
                return Ok(sync);
            }
            self.verified_sidecars
                .borrow_mut()
                .tables
                .remove(chunk_table);
        }
        let read = self
            .verified_sidecars
            .borrow()
            .readable_paths
            .get(chunk_table)
            .copied();
        let paired = match read {
            Some(ReadVerdict {
                readable: true,
                version: at,
            }) if at == version => true,
            Some(ReadVerdict { readable: true, .. }) => {
                self.verified_sidecars
                    .borrow_mut()
                    .readable_paths
                    .remove(chunk_table);
                false
            }
            _ => false,
        };
        let rebuilt = if paired {
            false
        } else {
            self.verified_sidecars.borrow_mut().probes += 1;
            sync_path_sidecar(&self.conn, chunk_table)?
        };
        if rebuilt {
            let mut memo = self.verified_sidecars.borrow_mut();
            memo.uncommitted_rebuild = true;
            memo.readable_paths.remove(chunk_table);
        }
        let fts_by_rowid = match fts_in_sync {
            Some(in_sync) => in_sync,
            None => {
                let fts = crate::schema::fts_table_name(chunk_table);
                self.table_exists(&fts)?
                    && fts_sidecar_health(&self.conn, chunk_table, &fts)?
                        == FtsSidecarHealth::InSync
            }
        };
        if !self.verified_sidecars.borrow().uncommitted_rebuild {
            self.mark_sidecars_verified(chunk_table, fts_by_rowid, version);
        }
        Ok(SidecarSync {
            fts_by_rowid,
            version,
        })
    }

    /// Once the transaction that reached `sync` has committed, remember it
    /// for writes and reads even if a rebuild ran in it: the committed state
    /// is paired, and `data_version` has not moved since that snapshot
    /// unless another connection committed after.
    pub(super) fn remember_committed_sidecars(&self, chunk_table: &str, sync: SidecarSync) {
        if !self.conn.is_autocommit() {
            return;
        }
        self.verified_sidecars.borrow_mut().uncommitted_rebuild = false;
        self.mark_sidecars_verified(chunk_table, sync.fts_by_rowid, sync.version);
        self.remember_readable_paths(chunk_table, true, sync.version);
    }

    /// `PRAGMA data_version` for the open transaction's snapshot. The pragma
    /// runs in a read transaction of its own, so in a deferred transaction
    /// that has not read yet it is the read that fixes the snapshot.
    pub(super) fn snapshot_data_version(&self) -> Result<i64> {
        #[cfg(test)]
        if self.conn.transaction_state(Some("main"))? == rusqlite::TransactionState::None {
            run_before_deferred_snapshot_hook();
        }
        Ok(self
            .conn
            .query_row("PRAGMA data_version", [], |row| row.get(0))?)
    }

    /// Whether this handle reads committed state: no transaction is open on
    /// `main`, or the open one has not written. A read snapshot on a
    /// writable handle qualifies; a failed state query does not.
    fn reads_committed_state(&self) -> bool {
        matches!(
            self.conn.transaction_state(Some("main")),
            Ok(rusqlite::TransactionState::None | rusqlite::TransactionState::Read)
        )
    }

    /// Remember a read verdict on `chunk_table`'s path sidecar reached at
    /// `version`, and offer it to the process-wide cache this handle was
    /// attached to when it describes committed state.
    pub(super) fn remember_readable_paths(&self, chunk_table: &str, readable: bool, version: i64) {
        let committed = self.reads_committed_state();
        let mut memo = self.verified_sidecars.borrow_mut();
        memo.readable_paths
            .insert(chunk_table.to_string(), ReadVerdict { readable, version });
        if committed && let Some(sink) = &memo.verdict_sink {
            sink.offer(chunk_table, readable);
        }
    }

    /// Record a sidecar state reached at `version`.
    pub(super) fn mark_sidecars_verified(
        &self,
        chunk_table: &str,
        fts_by_rowid: bool,
        version: i64,
    ) {
        self.verified_sidecars.borrow_mut().tables.insert(
            chunk_table.to_string(),
            SidecarSync {
                fts_by_rowid,
                version,
            },
        );
    }

    /// Delete `file_path`'s rows from `chunk_table`, its FTS sidecar, and its
    /// path sidecar, going by rowid through the path sidecar. The caller owns
    /// the savepoint and has run [`Database::ensure_sidecars`]; `fts` is
    /// `None` when the chunk table has no FTS sidecar. Returns the number of
    /// vec0 rows deleted.
    pub(super) fn delete_chunk_rows_for_path(
        &self,
        chunk_table: &str,
        file_path: &str,
        fts: Option<SidecarSync>,
    ) -> Result<usize> {
        let paths = quote_ident(&path_table_name(chunk_table));
        let ids = self
            .conn
            .prepare_cached(&format!("SELECT id FROM \"{paths}\" WHERE file_path = ?1"))?
            .query_map(params![file_path], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut count = 0;
        if !ids.is_empty() {
            // `id = ?` is vec0's point-lookup plan; `rowid = ?` would scan.
            let mut vec_stmt = self.conn.prepare_cached(&format!(
                "DELETE FROM \"{}\" WHERE id = ?1",
                quote_ident(chunk_table)
            ))?;
            for id in &ids {
                count += vec_stmt.execute(params![id])?;
            }
            self.conn
                .prepare_cached(&format!("DELETE FROM \"{paths}\" WHERE file_path = ?1"))?
                .execute(params![file_path])?;
        }
        if let Some(sync) = fts {
            let fts = quote_ident(&crate::schema::fts_table_name(chunk_table));
            if sync.fts_by_rowid {
                let mut fts_stmt = self
                    .conn
                    .prepare_cached(&format!("DELETE FROM \"{fts}\" WHERE rowid = ?1"))?;
                for id in &ids {
                    fts_stmt.execute(params![id])?;
                }
            } else {
                self.conn
                    .prepare_cached(&format!("DELETE FROM \"{fts}\" WHERE file_path = ?1"))?
                    .execute(params![file_path])?;
            }
        }
        Ok(count)
    }

    pub fn insert_chunks(
        &self,
        file_path: &str,
        language: &str,
        chunks: &[(&str, u32, u32, &[f32])],
    ) -> Result<()> {
        anyhow::ensure!(
            !self.chunk_table.is_empty(),
            "cannot insert chunks on a handle without a chunk table"
        );
        // Keep vec0, FTS5, and path-sidecar rowids atomic, including calls
        // inside an outer transaction.
        self.conn.execute_batch("SAVEPOINT insert_chunks")?;
        let result = (|| -> Result<SidecarSync> {
            let sync = self.ensure_sidecars(&self.chunk_table, None)?;
            let sql = format!(
                "INSERT INTO \"{}\"(file_path, language, content, start_line, end_line, embedding)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                quote_ident(&self.chunk_table)
            );
            let fts = self.fts_table();
            let fts_sql = format!(
                "INSERT INTO \"{}\"(rowid, content, file_path, language, start_line, end_line)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                quote_ident(&fts)
            );
            let paths_sql = format!(
                "INSERT INTO \"{}\"(id, file_path) VALUES (?1, ?2)",
                quote_ident(&path_table_name(&self.chunk_table))
            );
            let mut vec_stmt = self.conn.prepare(&sql)?;
            let mut fts_stmt = self.conn.prepare(&fts_sql)?;
            let mut paths_stmt = self.conn.prepare(&paths_sql)?;

            for (content, start_line, end_line, embedding) in chunks {
                let bytes = embedding_to_bytes(embedding);
                vec_stmt.execute(params![
                    file_path, language, content, start_line, end_line, bytes
                ])?;
                let rowid = self.conn.last_insert_rowid();
                fts_stmt.execute(params![
                    rowid, content, file_path, language, start_line, end_line
                ])?;
                paths_stmt.execute(params![rowid, file_path])?;
            }
            Ok(sync)
        })();
        match result {
            Ok(sync) => {
                self.conn.execute_batch("RELEASE insert_chunks")?;
                self.remember_committed_sidecars(&self.chunk_table, sync);
                Ok(())
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK TO insert_chunks");
                if let Err(re) = self.conn.execute_batch("RELEASE insert_chunks") {
                    tracing::warn!(error = %re, "failed to release savepoint insert_chunks after rollback");
                }
                Err(e)
            }
        }
    }

    pub fn delete_chunks_for_file(&self, file_path: &str) -> Result<usize> {
        if self.chunk_table.is_empty() {
            return Ok(0);
        }
        self.conn.execute_batch("SAVEPOINT delete_chunks")?;
        let result = self
            .ensure_sidecars(&self.chunk_table, None)
            .and_then(|sync| {
                let count =
                    self.delete_chunk_rows_for_path(&self.chunk_table, file_path, Some(sync))?;
                Ok((sync, count))
            });
        match result {
            Ok((sync, count)) => {
                self.conn.execute_batch("RELEASE delete_chunks")?;
                self.remember_committed_sidecars(&self.chunk_table, sync);
                Ok(count)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK TO delete_chunks");
                if let Err(re) = self.conn.execute_batch("RELEASE delete_chunks") {
                    tracing::warn!(error = %re, "failed to release savepoint delete_chunks after rollback");
                }
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
        if self.chunk_table.is_empty() {
            return Ok(Vec::new());
        }
        let t = quote_ident(&self.chunk_table);
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
        // vec0 rejects any ORDER BY beyond a bare `distance` on a KNN query,
        // so the tie-break is applied to the returned rows below rather than
        // in SQL.
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = if let Some(lang) = language {
            stmt.query_map(params![embedding_bytes, k as i64, lang], row_to_raw_search)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            stmt.query_map(params![embedding_bytes, k as i64], row_to_raw_search)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        rows.sort_by(|a, b| {
            a.distance
                .total_cmp(&b.distance)
                .then_with(|| a.file_path.cmp(&b.file_path))
                .then_with(|| a.start_line.cmp(&b.start_line))
                .then_with(|| a.end_line.cmp(&b.end_line))
        });
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
        if self.chunk_table.is_empty() {
            return Ok(Vec::new());
        }
        let t = quote_ident(&self.chunk_table);
        let mut conditions = Vec::new();
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        param_values.push(Box::new(embedding_bytes.to_vec()));

        if let Some(langs) = languages
            && !langs.is_empty()
        {
            push_language_filter(&mut conditions, &mut param_values, langs);
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
             ORDER BY distance, file_path, start_line, end_line
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

    pub fn filtered_chunk_count(
        &self,
        languages: Option<&[&str]>,
        paths: Option<&[&str]>,
    ) -> Result<usize> {
        if self.chunk_table.is_empty() {
            return Ok(0);
        }
        let t = quote_ident(&self.chunk_table);
        let mut conditions = Vec::new();
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(langs) = languages
            && !langs.is_empty()
        {
            push_language_filter(&mut conditions, &mut param_values, langs);
        }

        if let Some(path_patterns) = paths {
            push_path_glob(&mut conditions, &mut param_values, path_patterns);
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };
        let sql = format!("SELECT COUNT(*) FROM \"{t}\" {where_clause}");
        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            param_values.iter().map(|p| p.as_ref()).collect();
        let n: i64 = self
            .conn
            .query_row(&sql, params_refs.as_slice(), |row| row.get(0))?;
        Ok(n as usize)
    }

    pub fn chunk_count(&self) -> Result<usize> {
        if self.chunk_table.is_empty() {
            return Ok(0);
        }
        let sql = format!(
            "SELECT COUNT(*) FROM \"{}\"",
            quote_ident(&self.chunk_table)
        );
        let n: i64 = self.conn.query_row(&sql, [], |row| row.get(0))?;
        Ok(n as usize)
    }

    /// Distinct semantic files across all models; works on structural-only handles.
    pub fn semantic_file_count(&self) -> Result<usize> {
        if !self.semantic_file_schema_is_current()? {
            anyhow::bail!("semantic_files table is missing or malformed");
        }
        let n: i64 =
            self.conn
                .query_row("SELECT COUNT(DISTINCT path) FROM semantic_files", [], |r| {
                    r.get(0)
                })?;
        Ok(n as usize)
    }

    /// Count only this model's semantic files; old models remain until cleanup.
    pub fn semantic_file_count_for_model(&self, model: &str) -> Result<usize> {
        if !self.semantic_file_schema_is_current()? {
            anyhow::bail!("semantic_files table is missing or malformed");
        }
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(DISTINCT sf.path) FROM semantic_files sf
             JOIN semantic_models sm ON sm.chunk_table = sf.chunk_table WHERE sm.model = ?1",
            [model],
            |r| r.get(0),
        )?;
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
        if self.chunk_table.is_empty() {
            return Ok(());
        }
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
        if self.chunk_table.is_empty() {
            return Ok(());
        }
        self.conn.execute(
            "DELETE FROM semantic_files WHERE chunk_table = ?1 AND path = ?2",
            params![&self.chunk_table, path],
        )?;
        Ok(())
    }

    pub fn get_semantic_file_hash(&self, path: &str) -> Result<Option<String>> {
        if self.chunk_table.is_empty() {
            return Ok(None);
        }
        let hash = self
            .conn
            .query_row(
                "SELECT content_hash FROM semantic_files WHERE chunk_table = ?1 AND path = ?2",
                params![&self.chunk_table, path],
                |row| row.get(0),
            )
            .optional()?;
        Ok(hash)
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
    /// `languages` restricts rows to the given set (`language IN (...)`).
    /// `None` or an empty slice means no language filter.
    ///
    /// Query must be an FTS5 MATCH expression; pass pre-escaped. Callers that
    /// build a query from user input should go through a helper like
    /// `build_fts_match_query` to quote identifiers safely.
    pub fn search_bm25(
        &self,
        match_expr: &str,
        k: usize,
        languages: Option<&[&str]>,
        paths: Option<&[&str]>,
    ) -> Result<Vec<RawSearchRow>> {
        if self.chunk_table.is_empty() {
            return Ok(Vec::new());
        }
        let t = quote_ident(&self.fts_table());
        let mut conditions = vec![format!("\"{t}\" MATCH ?1")];
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> =
            vec![Box::new(match_expr.to_string())];

        if let Some(langs) = languages
            && !langs.is_empty()
        {
            push_language_filter(&mut conditions, &mut param_values, langs);
        }

        if let Some(path_patterns) = paths {
            push_path_glob(&mut conditions, &mut param_values, path_patterns);
        }

        let limit_idx = param_values.len() + 1;
        let sql = format!(
            "SELECT file_path, language, content, start_line, end_line, bm25(\"{t}\") AS score
             FROM \"{t}\"
             WHERE {}
             ORDER BY score, file_path, start_line, end_line LIMIT ?{limit_idx}",
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
        let fts = self.fts_table();
        let vocab = format!("{fts}_vocab");
        let vocab_ident = quote_ident(&vocab);
        let sql = format!("SELECT doc FROM \"{vocab_ident}\" WHERE term = ?1");
        let term = token.to_lowercase();
        let doc = self
            .conn
            .query_row(&sql, params![&term], |r| r.get::<_, i64>(0));
        let doc: Option<i64> = match doc {
            Ok(n) => Some(n),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(rusqlite::Error::SqliteFailure(_, Some(msg))) if msg.contains("no such table") => {
                return Ok((0, total));
            }
            Err(e) => return Err(e.into()),
        };
        Ok((doc.unwrap_or(0) as u64, total))
    }

    /// Total across all models, including on structural-only handles; zero if unindexed.
    pub fn total_chunk_count(&self) -> Result<usize> {
        let tables = self.list_vec_tables()?;
        let mut total: i64 = 0;
        for t in &tables {
            let sql = format!("SELECT COUNT(*) FROM \"{}\"", quote_ident(t));
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
        drop_chunk_table_group(&self.conn, table_name)?;
        let mut memo = self.verified_sidecars.borrow_mut();
        memo.tables.remove(table_name);
        memo.readable_paths.remove(table_name);
        if memo
            .seed
            .as_ref()
            .is_some_and(|seed| seed.at.chunk_table() == table_name)
        {
            memo.seed = None;
        }
        Ok(())
    }

    pub fn vacuum(&self) -> Result<()> {
        // VACUUM needs a temporary copy. This advisory headroom check is skipped
        // when filesystem statistics are unavailable; it cannot guarantee completion.
        if let Some(path) = self.conn.path().filter(|p| !p.is_empty())
            && let Ok(meta) = std::fs::metadata(path)
            && let Some(available) = available_disk_space(path)
        {
            let required = (meta.len() as f64 * 1.2) as u64;
            if available < required {
                anyhow::bail!(
                    "not enough free disk space for VACUUM: database is {} bytes and \
                     VACUUM can need up to ~2x that; {} bytes available, {} required — \
                     free up space and retry",
                    meta.len(),
                    available,
                    required
                );
            }
        }
        self.conn.execute_batch("VACUUM")?;
        Ok(())
    }

    /// `(content, embedding)` for every chunk of `file_path`, so a re-index
    /// of an edited file can keep the vectors of chunks whose text did not
    /// change instead of embedding the whole file again.
    pub fn chunk_embeddings_for_file(&self, file_path: &str) -> Result<Vec<(String, Vec<f32>)>> {
        if self.chunk_table.is_empty() {
            return Ok(Vec::new());
        }
        let _snapshot = self.verdict_snapshot()?;
        let sql = self.per_file_chunk_sql("c.content, c.embedding", "")?;
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt
            .query_map(params![file_path], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(content, bytes)| Ok((content, embedding_from_bytes(&bytes)?)))
            .collect()
    }

    pub fn chunks_for_file(&self, file_path: &str) -> Result<Vec<RawSearchRow>> {
        if let Some(chunks) = self.overlay_source_chunks(file_path)? {
            return Ok(chunks);
        }
        if self.chunk_table.is_empty() {
            return Ok(Vec::new());
        }
        let _snapshot = self.verdict_snapshot()?;
        let sql = self.per_file_chunk_sql(
            "c.file_path, c.language, c.content, c.start_line, c.end_line",
            "ORDER BY c.start_line",
        )?;
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt
            .query_map(params![file_path], |row| {
                Ok(RawSearchRow {
                    retrieval: Default::default(),
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

    /// SQL selecting `columns` of `?1`'s rows in the chunk table, aliased `c`.
    ///
    /// A `file_path` constraint on vec0 is a full scan that prepares one
    /// auxiliary-column query per row, so a trusted path sidecar supplies the
    /// ids and vec0 serves each by its `id` point plan. The `c.file_path`
    /// recheck keeps a sidecar whose ids name other paths (left by a binary
    /// that repopulated the chunk table without it) from attributing another
    /// file's chunks; an untrusted or absent sidecar falls back to the scan.
    pub(super) fn per_file_chunk_sql(&self, columns: &str, order: &str) -> Result<String> {
        let chunk = quote_ident(&self.chunk_table);
        Ok(if self.path_sidecar_readable(&self.chunk_table)? {
            let paths = quote_ident(&path_table_name(&self.chunk_table));
            format!(
                "SELECT {columns} FROM \"{paths}\" AS p CROSS JOIN \"{chunk}\" AS c
                 ON c.id = p.id
                 WHERE p.file_path = ?1 AND c.file_path = ?1 {order}"
            )
        } else {
            format!("SELECT {columns} FROM \"{chunk}\" AS c WHERE c.file_path = ?1 {order}")
        })
    }

    /// Whether `chunk_table`'s path sidecar holds exactly its `(id, path)`
    /// pairs, established without writing so read-only and query-path
    /// handles can use it. [`Database::ensure_sidecars`] records the same
    /// verdict. A negative verdict stands until a committed chunk write
    /// through this handle, whose own check proves the pairs, replaces it;
    /// the scan it selects is always correct meanwhile. A positive one
    /// is reused only inside a transaction whose snapshot reads the
    /// `data_version` it was reached at, and is remembered only while no
    /// transaction on this handle has written; this handle's later writes
    /// keep both sides paired.
    pub(super) fn path_sidecar_readable(&self, chunk_table: &str) -> Result<bool> {
        let snapshot = if self.conn.is_autocommit() {
            None
        } else {
            Some(self.snapshot_data_version()?)
        };
        let remembered = self
            .verified_sidecars
            .borrow()
            .readable_paths
            .get(chunk_table)
            .copied();
        match remembered {
            Some(ReadVerdict {
                readable: false, ..
            }) => return Ok(false),
            Some(ReadVerdict {
                readable: true,
                version,
            }) if Some(version) == snapshot => return Ok(true),
            Some(_) => {
                self.verified_sidecars
                    .borrow_mut()
                    .readable_paths
                    .remove(chunk_table);
            }
            None => {}
        }
        if let Some(readable) = self.confirmed_seed(chunk_table)? {
            return Ok(readable);
        }
        // Outside a transaction the probe's statements take later snapshots
        // than this read; a commit in between leaves the verdict newer than
        // its version, which only costs a later re-probe.
        let version = match snapshot {
            Some(version) => version,
            None => self
                .conn
                .query_row("PRAGMA data_version", [], |row| row.get(0))?,
        };
        self.verified_sidecars.borrow_mut().probes += 1;
        let readable =
            path_sidecar_matches_auxiliary(&self.conn, chunk_table, &path_table_name(chunk_table))?;
        // Inside a transaction that has written, a positive verdict could
        // rest on writes a rollback undoes, so only committed state is
        // remembered.
        if !readable || self.reads_committed_state() {
            self.remember_readable_paths(chunk_table, readable, version);
        }
        Ok(readable)
    }

    /// How many path-sidecar pair probes this handle has run. Exists only so
    /// tests in dependent crates can see that a cached verdict spared one;
    /// not a supported surface.
    #[doc(hidden)]
    pub fn path_sidecar_probes_for_tests(&self) -> u32 {
        self.verified_sidecars.borrow().probes
    }

    /// A read snapshot for a per-file read opened outside any transaction:
    /// a remembered or seeded verdict is confirmed against a snapshot, and
    /// without one the confirmation and the read would each take their own,
    /// with room for a commit between them.
    fn verdict_snapshot(&self) -> Result<Option<super::ReadSnapshot<'_>>> {
        if !self.conn.is_autocommit() {
            return Ok(None);
        }
        Ok(Some(self.read_snapshot()?))
    }

    /// The seeded verdict for `chunk_table`, if this handle holds one that
    /// still describes the snapshot it reads. Only a read transaction that
    /// has not written qualifies: its WAL snapshot is fixed once taken, and a
    /// snapshot taken after the attach while the observer still reads the
    /// attach version is exactly that state. A deferred `BEGIN` takes its
    /// snapshot at the first read, so one is forced before the observer is
    /// consulted. A seed that fails the check is dropped for good.
    fn confirmed_seed(&self, chunk_table: &str) -> Result<Option<bool>> {
        use rusqlite::TransactionState;
        let readable = match &self.verified_sidecars.borrow().seed {
            Some(seed) if seed.at.chunk_table() == chunk_table => seed.readable,
            _ => return Ok(None),
        };
        if self.conn.is_autocommit() {
            return Ok(None);
        }
        self.take_deferred_snapshot()?;
        if self.conn.transaction_state(Some("main"))? != TransactionState::Read {
            return Ok(None);
        }
        let unchanged = self
            .verified_sidecars
            .borrow()
            .seed
            .as_ref()
            .is_some_and(|seed| seed.at.unchanged());
        if unchanged {
            return Ok(Some(readable));
        }
        self.verified_sidecars.borrow_mut().seed = None;
        Ok(None)
    }

    /// Fix the WAL snapshot of a transaction opened by a deferred `BEGIN`
    /// that has not read yet; no-op once it has.
    fn take_deferred_snapshot(&self) -> Result<()> {
        if self.conn.transaction_state(Some("main"))? != rusqlite::TransactionState::None {
            return Ok(());
        }
        #[cfg(test)]
        run_before_deferred_snapshot_hook();
        self.conn
            .query_row("SELECT count(*) FROM sqlite_master", [], |row| {
                row.get::<_, i64>(0)
            })?;
        Ok(())
    }

    /// Distinct `file_path` values of the chunk table, sorted. A trusted path
    /// sidecar serves them from its `file_path` index instead of a vec0 scan
    /// that prepares one auxiliary query per row. The sidecar stores a NULL
    /// path as `''`, which the vec0 read reports as a type error and a real
    /// empty path as `""`; a sidecar holding `''` cannot tell the two apart,
    /// so that case keeps the scan.
    pub fn all_chunk_file_paths(&self) -> Result<Vec<String>> {
        if self.chunk_table.is_empty() {
            return Ok(Vec::new());
        }
        let _snapshot = self.verdict_snapshot()?;
        let from_sidecar = self.path_sidecar_readable(&self.chunk_table)?;
        let mut paths = self.distinct_chunk_file_paths(from_sidecar)?;
        if from_sidecar && paths.first().is_some_and(String::is_empty) {
            paths = self.distinct_chunk_file_paths(false)?;
        }
        Ok(paths)
    }

    fn distinct_chunk_file_paths_sql(&self, from_sidecar: bool) -> String {
        let table = if from_sidecar {
            path_table_name(&self.chunk_table)
        } else {
            self.chunk_table.clone()
        };
        format!(
            "SELECT DISTINCT file_path FROM \"{}\" ORDER BY file_path",
            quote_ident(&table)
        )
    }

    fn distinct_chunk_file_paths(&self, from_sidecar: bool) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare(&self.distinct_chunk_file_paths_sql(from_sidecar))?;
        let paths = stmt
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(paths)
    }

    /// Identity key for process-lifetime caches over this handle's semantic
    /// content: the database file path plus the active chunk table. `None`
    /// for in-memory or table-less handles: every in-memory connection is a
    /// distinct database, so a path-keyed cache entry would alias unrelated
    /// handles.
    pub fn semantic_cache_key(&self) -> Option<(String, String)> {
        if self.chunk_table.is_empty() {
            return None;
        }
        match self.conn.path() {
            Some(p) if !p.is_empty() => Some((p.to_string(), self.chunk_table.clone())),
            _ => None,
        }
    }

    /// Heuristic path-set token for the active table; reads semantic_files, not vec0.
    /// Path-length and first-character sums distinguish same-second delete/insert
    /// batches that reuse the highest rowid and preserve count/indexed_at.
    /// Renames preserving both sums can still collide; same-second content-only
    /// re-embeds may also collide, so use only for path-set caches.
    pub fn semantic_files_validity_token(&self) -> Result<SemanticValidityToken> {
        let token = self.conn.query_row(
            "SELECT COUNT(*), COALESCE(MAX(rowid), 0), COALESCE(MAX(indexed_at), 0),
                    COALESCE(SUM(length(path)), 0), COALESCE(SUM(unicode(path)), 0)
             FROM semantic_files WHERE chunk_table = ?1",
            params![&self.chunk_table],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?;
        Ok(token)
    }
}

#[cfg(test)]
mod tests {
    use crate::Database;

    #[test]
    fn model_coverage_matches_exact_identity() {
        let db = Database::open_in_memory().unwrap();
        for (table, model, path) in [
            ("chunks_org_model_2", "org/model", "shared.rs"),
            ("chunks_org_model_3", "org/model", "shared.rs"),
            ("chunks_org_model_3", "org/model", "own.rs"),
            ("chunks_org_model_extra_2", "org/model-extra", "longer.rs"),
            ("chunks_org_model_4", "org-model", "punctuation.rs"),
            ("chunks_orgXmodel_2", "orgXmodel", "wildcard.rs"),
        ] {
            db.conn.execute(
                "INSERT OR IGNORE INTO semantic_models (chunk_table, model, dim) VALUES (?1, ?2, 2)",
                rusqlite::params![table, model],
            ).unwrap();
            db.conn.execute(
                "INSERT INTO semantic_files (chunk_table, path, content_hash) VALUES (?1, ?2, 'hash')",
                rusqlite::params![table, path],
            ).unwrap();
        }
        assert_eq!(db.semantic_file_count_for_model("org/model").unwrap(), 2);
        assert_eq!(
            db.semantic_file_count_for_model("org/model-extra").unwrap(),
            1
        );
        assert_eq!(db.semantic_file_count_for_model("org-model").unwrap(), 1);
        assert_eq!(db.semantic_file_count_for_model("orgXmodel").unwrap(), 1);
        assert_eq!(db.semantic_file_count_for_model("org").unwrap(), 0);
        assert_eq!(db.semantic_file_count_for_model("org.model").unwrap(), 0);
    }

    #[test]
    fn search_row_accepts_numeric_distance() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let row = conn
            .query_row(
                "SELECT 'src/query.rs', 'rust', 'fn query() {}', 3, 5, 0.375 AS distance",
                [],
                super::row_to_raw_search,
            )
            .unwrap();
        assert_eq!(row.file_path, "src/query.rs");
        assert_eq!(row.distance, 0.375);
    }

    #[test]
    fn search_rows_reject_null_distance_without_partial_success() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT 'src/first.rs', 'rust', 'fn first() {}', 1, 1, 0.25 AS distance
                 UNION ALL SELECT 'src/second.rs', 'rust', 'fn second() {}', 1, 1, NULL",
            )
            .unwrap();
        let error = stmt
            .query_map([], super::row_to_raw_search)
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap_err();
        assert!(
            matches!(
                error,
                rusqlite::Error::InvalidColumnType(5, ref name, rusqlite::types::Type::Null)
                    if name == "distance"
            ),
            "NULL distance must identify the failing column: {error}"
        );
    }

    #[test]
    fn vacuum_succeeds_on_file_backed_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vacuum.db");
        let db = Database::open(&path).unwrap();
        db.vacuum()
            .expect("vacuum with ample free space must pass the pre-flight");
    }

    #[test]
    fn vacuum_succeeds_on_in_memory_db() {
        let db = Database::open_in_memory().unwrap();
        db.vacuum()
            .expect("in-memory vacuum must skip the pre-flight");
    }

    #[test]
    fn validity_token_differs_when_max_rowid_row_swapped_for_new_path() {
        let db = Database::open_in_memory().unwrap();
        db.upsert_semantic_file_hash("aa", "h").unwrap();
        db.upsert_semantic_file_hash("bbbb", "h").unwrap(); // highest rowid
        // Pin indexed_at so MAX(indexed_at) can't be what distinguishes the
        // tokens, isolating the collision to count/rowid.
        db.conn
            .execute("UPDATE semantic_files SET indexed_at = 1000", [])
            .unwrap();
        let before = db.semantic_files_validity_token().unwrap();

        // Reusing the highest rowid preserves the old three-component token.
        db.delete_semantic_file_hash("bbbb").unwrap();
        db.upsert_semantic_file_hash("ccccccc", "h").unwrap();
        db.conn
            .execute("UPDATE semantic_files SET indexed_at = 1000", [])
            .unwrap();
        let after = db.semantic_files_validity_token().unwrap();

        assert_eq!(
            (before.0, before.1, before.2),
            (after.0, after.1, after.2),
            "count/max_rowid/max_indexed_at must collide — the exact case the old token missed"
        );
        assert_ne!(
            before, after,
            "path-content components must break the collision"
        );
    }
}
