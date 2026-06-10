use std::sync::Once;

use rusqlite::Connection;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS files (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    language TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    indexed_at INTEGER NOT NULL DEFAULT (unixepoch()),
    -- Unix epoch of the last trust-boundary derivation, or 0 when never
    -- derived. Lets the targeted backfill distinguish "rule-clean empty
    -- set" from "never-derived empty set"; updated by every
    -- `replace_file_trust_boundaries` call.
    boundaries_derived_at INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS symbols (
    id INTEGER PRIMARY KEY,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    qualified_name TEXT NOT NULL,
    kind TEXT NOT NULL,
    line_start INTEGER NOT NULL,
    line_end INTEGER NOT NULL,
    col_start INTEGER NOT NULL,
    col_end INTEGER NOT NULL,
    rationale TEXT NOT NULL DEFAULT '[]'
);

CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
CREATE INDEX IF NOT EXISTS idx_symbols_qualified ON symbols(qualified_name);
CREATE INDEX IF NOT EXISTS idx_symbols_file ON symbols(file_id);

CREATE TABLE IF NOT EXISTS refs (
    id INTEGER PRIMARY KEY,
    from_file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    from_symbol TEXT,
    to_name TEXT NOT NULL,
    to_name_tail TEXT NOT NULL DEFAULT '',
    kind TEXT NOT NULL,
    line INTEGER NOT NULL,
    col INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_refs_to_name ON refs(to_name);
CREATE INDEX IF NOT EXISTS idx_refs_from_file ON refs(from_file_id);

CREATE TABLE IF NOT EXISTS git_files (
    path TEXT PRIMARY KEY,
    churn_score REAL NOT NULL DEFAULT 0,
    fix_count INTEGER NOT NULL DEFAULT 0,
    total_commits INTEGER NOT NULL DEFAULT 0,
    last_commit_at INTEGER,
    indexed_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE INDEX IF NOT EXISTS idx_git_files_churn ON git_files(churn_score DESC);

CREATE TABLE IF NOT EXISTS git_co_changes (
    file_a TEXT NOT NULL,
    file_b TEXT NOT NULL,
    weight REAL NOT NULL DEFAULT 0,
    count INTEGER NOT NULL DEFAULT 0,
    last_observed_at INTEGER,
    PRIMARY KEY (file_a, file_b)
);

CREATE INDEX IF NOT EXISTS idx_git_co_changes_file_a ON git_co_changes(file_a, weight DESC);
CREATE INDEX IF NOT EXISTS idx_git_co_changes_file_b ON git_co_changes(file_b, weight DESC);

CREATE TABLE IF NOT EXISTS git_index_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    last_sha TEXT,
    last_indexed_at INTEGER
);

CREATE TABLE IF NOT EXISTS structural_index_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    last_sha TEXT,
    last_indexed_at INTEGER
);

CREATE TABLE IF NOT EXISTS semantic_files (
    chunk_table TEXT NOT NULL,
    path TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    indexed_at INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (chunk_table, path)
);

CREATE INDEX IF NOT EXISTS idx_semantic_files_path ON semantic_files(path);

CREATE TABLE IF NOT EXISTS semantic_models (
    chunk_table TEXT PRIMARY KEY,
    model TEXT NOT NULL,
    dim INTEGER NOT NULL,
    indexed_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE INDEX IF NOT EXISTS idx_semantic_models_model ON semantic_models(model);

CREATE TABLE IF NOT EXISTS file_trust_boundaries (
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    boundary TEXT NOT NULL,
    PRIMARY KEY (file_id, boundary)
);

CREATE INDEX IF NOT EXISTS idx_file_trust_boundaries_boundary
    ON file_trust_boundaries(boundary);

CREATE TABLE IF NOT EXISTS features (
    feature_id    TEXT PRIMARY KEY,
    title         TEXT NOT NULL,
    summary       TEXT NOT NULL,
    kind          TEXT NOT NULL,
    source        TEXT NOT NULL,
    confidence    TEXT NOT NULL,
    entry_path    TEXT NOT NULL,
    entry_symbol  TEXT,
    entry_route   TEXT,
    entry_command TEXT,
    language      TEXT NOT NULL,
    tags          TEXT NOT NULL DEFAULT '[]',
    created_at    INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at    INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE INDEX IF NOT EXISTS idx_features_kind     ON features(kind);
CREATE INDEX IF NOT EXISTS idx_features_language ON features(language);
CREATE INDEX IF NOT EXISTS idx_features_source   ON features(source);

CREATE TABLE IF NOT EXISTS feature_files (
    feature_id TEXT NOT NULL REFERENCES features(feature_id) ON DELETE CASCADE,
    path       TEXT NOT NULL,
    role       TEXT NOT NULL,
    reason     TEXT,
    PRIMARY KEY (feature_id, path, role)
);

CREATE INDEX IF NOT EXISTS idx_feature_files_path ON feature_files(path);

CREATE TABLE IF NOT EXISTS feature_trust_boundaries (
    feature_id TEXT NOT NULL REFERENCES features(feature_id) ON DELETE CASCADE,
    boundary   TEXT NOT NULL,
    PRIMARY KEY (feature_id, boundary)
);
"#;

pub fn semantic_schema(table_name: &str, dim: usize) -> String {
    format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS \"{table_name}\" USING vec0(\
         id INTEGER PRIMARY KEY, \
         +file_path TEXT, \
         language TEXT partition key, \
         +content TEXT, \
         +start_line INTEGER, \
         +end_line INTEGER, \
         embedding float[{dim}]);"
    )
}

/// Name of the FTS5 sidecar for a given chunk table. Synced row-for-row with
/// the vec0 table during `insert_chunks`; used by the gated hybrid BM25 path.
/// Keeping the name a mechanical suffix means one `ensure_chunk_table` call
/// provisions both sides together.
pub fn fts_table_name(chunk_table: &str) -> String {
    format!("{chunk_table}_fts")
}

/// DDL for the FTS5 sidecar. `tokenchars '_'` keeps identifiers like
/// `doc_cfg`, `mb_convert_case`, `moduleref` intact instead of splitting
/// them into half-useful tokens. No Porter stemmer — we match code
/// identifiers verbatim, not English.
pub fn fts_schema(table_name: &str) -> String {
    format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS \"{table_name}\" USING fts5(\
         content, \
         file_path UNINDEXED, \
         language UNINDEXED, \
         start_line UNINDEXED, \
         end_line UNINDEXED, \
         tokenize = \"unicode61 remove_diacritics 1 tokenchars '_'\");"
    )
}

fn quote_ident(identifier: &str) -> String {
    identifier.replace('"', "\"\"")
}

fn table_row_count(conn: &Connection, table_name: &str) -> rusqlite::Result<i64> {
    let sql = format!("SELECT COUNT(*) FROM \"{}\"", quote_ident(table_name));
    conn.query_row(&sql, [], |row| row.get(0))
}

fn repair_fts_sidecar(
    conn: &Connection,
    chunk_table: &str,
    fts_table: &str,
) -> rusqlite::Result<()> {
    let chunk_count = table_row_count(conn, chunk_table)?;
    let fts_count = table_row_count(conn, fts_table)?;
    if chunk_count == fts_count {
        return Ok(());
    }

    let chunk_table = quote_ident(chunk_table);
    let fts_table = quote_ident(fts_table);
    // Wrap DELETE + INSERT…SELECT in one transaction. Without it, a crash
    // between the two statements leaves the FTS sidecar empty while the
    // chunk table still has data; BM25 search returns zero results until
    // the next process start re-runs the repair.
    let sql = format!(
        "BEGIN;
         DELETE FROM \"{fts_table}\";
         INSERT INTO \"{fts_table}\"(rowid, content, file_path, language, start_line, end_line)
         SELECT id, content, file_path, language, start_line, end_line
         FROM \"{chunk_table}\"
         ORDER BY id;
         COMMIT;"
    );
    if let Err(e) = conn.execute_batch(&sql) {
        let _ = conn.execute_batch("ROLLBACK");
        return Err(e);
    }
    Ok(())
}

pub fn model_table_name(model: &str, dim: usize) -> String {
    format!("{}{dim}", model_table_prefix(model))
}

pub fn model_table_prefix(model: &str) -> String {
    let sanitized: String = model
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    format!("chunks_{sanitized}_")
}

unsafe extern "C" {
    fn sqlite3_vec_init(
        db: *mut rusqlite::ffi::sqlite3,
        pz_err_msg: *mut *mut std::ffi::c_char,
        p_api: *const rusqlite::ffi::sqlite3_api_routines,
    ) -> std::ffi::c_int;
}

static VEC_INIT: Once = Once::new();

pub fn init_vec_extension() {
    // SAFETY: sqlite-vec exposes a valid SQLite extension entrypoint with the
    // signature required by `sqlite3_auto_extension`, and registration is
    // process-global/idempotent behind `Once`.
    VEC_INIT.call_once(|| unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(sqlite3_vec_init));
    });
}

pub fn init_db(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;
    // Wait up to 5s for a competing writer instead of failing immediately
    // with `SQLITE_BUSY`. The advisory lockfile (see graph::indexing) already
    // serializes writers, but a second MCP session reading mid-index would
    // otherwise hit instant-busy on the brief WAL-checkpoint windows. Match
    // repowise's posture (see notes/2026-04-29 sweep, §1.8).
    conn.execute_batch("PRAGMA busy_timeout=5000;")?;
    // synchronous=NORMAL is the documented safe pairing with WAL: fsync only at
    // checkpoint, not on every commit. Durability across a power loss is
    // unchanged for WAL (only the last transaction(s) since the last checkpoint
    // can be lost, and the DB stays consistent) — and this is a derived index,
    // rebuildable from source, so the trade is firmly worth it for indexer
    // commit throughput. mmap_size and a larger page cache cut syscall and
    // page-fault overhead on the read-heavy search path (KNN + chunk rows).
    // negative cache_size is in KiB; -65536 ≈ 64 MiB. mmap is backed by the OS
    // page cache, so it doesn't pin RSS.
    conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
    conn.execute_batch("PRAGMA mmap_size=268435456;")?;
    conn.execute_batch("PRAGMA cache_size=-65536;")?;
    conn.execute_batch(SCHEMA)?;
    run_migrations(conn)?;
    Ok(())
}

/// Hard rule for anyone adding a new migration to [`MIGRATIONS`]: the `up` body
/// must be **safe when run against an already-current schema**. On a fresh DB,
/// [`init_db`] creates the latest `SCHEMA` first, then still runs every entry
/// in [`MIGRATIONS`] to record them in `schema_migrations` (registry + fresh
/// schema are decoupled). Each migration therefore needs to self-check its
/// target state (existence of a column, index, or row) and no-op if already
/// applied. The existing `0001_refs_name_tail` migration is the template.
///
/// `up` functions do NOT need to open their own transactions; the runner opens
/// one transaction per migration and records the migration in
/// `schema_migrations` within that same transaction, so either both land or
/// neither does.
type MigrationUp = fn(&Connection) -> rusqlite::Result<()>;

const MIGRATIONS: &[(&str, MigrationUp)] = &[
    ("0001_refs_name_tail", migrate_0001_refs_name_tail),
    (
        "0002_structural_index_state",
        migrate_0002_structural_index_state,
    ),
    ("0003_semantic_files", migrate_0003_semantic_files),
    (
        "0004_semantic_files_chunk_table",
        migrate_0004_semantic_files_chunk_table,
    ),
    ("0005_semantic_models", migrate_0005_semantic_models),
    ("0006_refs_name_tail_dot", migrate_0006_refs_name_tail_dot),
    ("0007_symbols_rationale", migrate_0007_symbols_rationale),
    (
        "0008_file_trust_boundaries",
        migrate_0008_file_trust_boundaries,
    ),
    ("0009_feature_tables", migrate_0009_feature_tables),
    (
        "0010_files_boundaries_derived_at",
        migrate_0010_files_boundaries_derived_at,
    ),
    (
        "0011_features_test_command",
        migrate_0011_features_test_command,
    ),
];

fn run_migrations(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
             id INTEGER PRIMARY KEY,
             name TEXT NOT NULL UNIQUE,
             applied_at INTEGER NOT NULL DEFAULT (unixepoch())
         );",
    )?;
    for (name, up) in MIGRATIONS {
        let already: i64 = conn.query_row(
            "SELECT COUNT(*) FROM schema_migrations WHERE name = ?1",
            rusqlite::params![name],
            |r| r.get(0),
        )?;
        if already > 0 {
            continue;
        }
        conn.execute_batch("BEGIN")?;
        if let Err(e) = (|| -> rusqlite::Result<()> {
            up(conn)?;
            conn.execute(
                "INSERT INTO schema_migrations (name) VALUES (?1)",
                rusqlite::params![name],
            )?;
            Ok(())
        })() {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(e);
        }
        conn.execute_batch("COMMIT")?;
    }
    Ok(())
}

/// Extract the trailing segment of a qualified name past the last `\`, `/`, `.`, or `::`.
/// PHP `App\Http\Controllers\Foo` → `Foo`; Rust `mod::sub::bar` → `bar`; Go `fmt.Println` → `Println`.
pub fn name_tail(s: &str) -> &str {
    let mut best: Option<usize> = None;
    if let Some(p) = s.rfind('\\') {
        best = Some(p + 1);
    }
    if let Some(p) = s.rfind('/') {
        best = Some(best.map_or(p + 1, |b| b.max(p + 1)));
    }
    if let Some(p) = s.rfind('.') {
        best = Some(best.map_or(p + 1, |b| b.max(p + 1)));
    }
    if let Some(p) = s.rfind("::") {
        best = Some(best.map_or(p + 2, |b| b.max(p + 2)));
    }
    match best {
        Some(p) => &s[p..],
        None => s,
    }
}

/// Adds `refs.to_name_tail` + backfill + supporting index. Safe against
/// already-current schema: guarded by `pragma_table_info` check. Runner owns
/// the transaction; this body can issue SQL directly.
fn migrate_0001_refs_name_tail(conn: &Connection) -> rusqlite::Result<()> {
    let has_column: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('refs') WHERE name = 'to_name_tail'",
        [],
        |row| row.get(0),
    )?;
    if has_column == 0 {
        conn.execute_batch("ALTER TABLE refs ADD COLUMN to_name_tail TEXT NOT NULL DEFAULT '';")?;
        let rows: Vec<(i64, String)> = {
            let mut stmt = conn.prepare("SELECT id, to_name FROM refs")?;
            stmt.query_map([], |row| Ok((row.get(0)?, row.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut update = conn.prepare("UPDATE refs SET to_name_tail = ?1 WHERE id = ?2")?;
        for (id, to_name) in &rows {
            update.execute(rusqlite::params![name_tail(to_name), id])?;
        }
    }
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_refs_to_name_tail ON refs(to_name_tail);")?;
    Ok(())
}

/// Adds `files.boundaries_derived_at` (epoch seconds; 0 = never derived).
/// The marker distinguishes "rule-clean file" from "never-derived file":
/// without it, an empty `file_trust_boundaries` rowset is indistinguishable
/// across the two states and the backfill can't target stragglers
/// precisely. Existing rows default to 0 so they get picked up on the
/// next index pass.
fn migrate_0010_files_boundaries_derived_at(conn: &Connection) -> rusqlite::Result<()> {
    let has_column: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('files') WHERE name = 'boundaries_derived_at'",
        [],
        |row| row.get(0),
    )?;
    if has_column == 0 {
        conn.execute_batch(
            "ALTER TABLE files ADD COLUMN boundaries_derived_at INTEGER NOT NULL DEFAULT 0;",
        )?;
    }
    Ok(())
}

/// Adds `features.test_command` as a free-form shell-command column. Lets
/// the mapper surface a runnable test invocation (e.g. `pnpm --dir
/// packages/api test`, `go test ./pkg/util/...`, `uv run pytest`) without
/// abusing `entry_command` — which is documented as an argv[0]-shape
/// token and contributes to the feature-id hash. Test commands routinely
/// change as the project's package-manager / lockfile evolves, so they
/// must not destabilize feature identity. Existing rows default to NULL
/// and get populated on the next `codesage index` mapper pass.
fn migrate_0011_features_test_command(conn: &Connection) -> rusqlite::Result<()> {
    let has_column: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('features') WHERE name = 'test_command'",
        [],
        |row| row.get(0),
    )?;
    if has_column == 0 {
        conn.execute_batch("ALTER TABLE features ADD COLUMN test_command TEXT;")?;
    }
    Ok(())
}

/// Adds the three feature-mapping tables: `features` (one row per
/// behavior-keyed slice), `feature_files` (junction with role tag), and
/// `feature_trust_boundaries` (aggregated boundary tags per feature).
/// Idempotent via `IF NOT EXISTS`; existing indexes get the tables empty
/// and pick up real rows on the next `codesage map` pass.
fn migrate_0009_feature_tables(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS features (
             feature_id    TEXT PRIMARY KEY,
             title         TEXT NOT NULL,
             summary       TEXT NOT NULL,
             kind          TEXT NOT NULL,
             source        TEXT NOT NULL,
             confidence    TEXT NOT NULL,
             entry_path    TEXT NOT NULL,
             entry_symbol  TEXT,
             entry_route   TEXT,
             entry_command TEXT,
             language      TEXT NOT NULL,
             tags          TEXT NOT NULL DEFAULT '[]',
             created_at    INTEGER NOT NULL DEFAULT (unixepoch()),
             updated_at    INTEGER NOT NULL DEFAULT (unixepoch())
         );",
    )?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_features_kind     ON features(kind);
         CREATE INDEX IF NOT EXISTS idx_features_language ON features(language);
         CREATE INDEX IF NOT EXISTS idx_features_source   ON features(source);",
    )?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS feature_files (
             feature_id TEXT NOT NULL REFERENCES features(feature_id) ON DELETE CASCADE,
             path       TEXT NOT NULL,
             role       TEXT NOT NULL,
             reason     TEXT,
             PRIMARY KEY (feature_id, path, role)
         );
         CREATE INDEX IF NOT EXISTS idx_feature_files_path ON feature_files(path);",
    )?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS feature_trust_boundaries (
             feature_id TEXT NOT NULL REFERENCES features(feature_id) ON DELETE CASCADE,
             boundary   TEXT NOT NULL,
             PRIMARY KEY (feature_id, boundary)
         );",
    )?;
    Ok(())
}

/// Adds `file_trust_boundaries` table for per-file trust-boundary tags (the
/// new term feeding `assess_risk`). Idempotent via `IF NOT EXISTS`; existing
/// indexes pick up boundary rows on the next file-touch + reindex (or via
/// `features::derive_for_index` against the live DB). No backfill in the
/// migration body itself — the next index pass writes real rows and the risk
/// score reads them; a file with no row simply contributes a zero
/// trust-boundary term, preserving the prior behavior until rederivation.
fn migrate_0008_file_trust_boundaries(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS file_trust_boundaries (
             file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
             boundary TEXT NOT NULL,
             PRIMARY KEY (file_id, boundary)
         );",
    )?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_file_trust_boundaries_boundary
             ON file_trust_boundaries(boundary);",
    )?;
    Ok(())
}

/// Adds `symbols.rationale` (JSON-encoded `Vec<RationaleEntry>`) for storing
/// decision-shape comments attached to symbol definitions. Defaults to `'[]'`
/// so existing rows behave like "no rationale extracted yet"; the next
/// indexer pass over a file refreshes its symbols and writes real values.
fn migrate_0007_symbols_rationale(conn: &Connection) -> rusqlite::Result<()> {
    let has_column: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('symbols') WHERE name = 'rationale'",
        [],
        |row| row.get(0),
    )?;
    if has_column == 0 {
        conn.execute_batch("ALTER TABLE symbols ADD COLUMN rationale TEXT NOT NULL DEFAULT '[]';")?;
    }
    Ok(())
}

fn migrate_0006_refs_name_tail_dot(conn: &Connection) -> rusqlite::Result<()> {
    let has_column: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('refs') WHERE name = 'to_name_tail'",
        [],
        |row| row.get(0),
    )?;
    if has_column == 0 {
        return Ok(());
    }

    let rows: Vec<(i64, String)> = {
        let mut stmt = conn.prepare("SELECT id, to_name FROM refs")?;
        stmt.query_map([], |row| Ok((row.get(0)?, row.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut update = conn.prepare("UPDATE refs SET to_name_tail = ?1 WHERE id = ?2")?;
    for (id, to_name) in &rows {
        update.execute(rusqlite::params![name_tail(to_name), id])?;
    }
    Ok(())
}

/// Adds `structural_index_state` for tracking the last HEAD SHA the structural
/// index was built against. Parallel shape to `git_index_state` and safe on a
/// current schema (guarded by `IF NOT EXISTS`).
fn migrate_0002_structural_index_state(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS structural_index_state (
             id INTEGER PRIMARY KEY CHECK (id = 1),
             last_sha TEXT,
             last_indexed_at INTEGER
         );",
    )?;
    Ok(())
}

/// Adds per-file semantic freshness state. Structural indexing can run with
/// `--no-semantic`; semantic indexing therefore needs its own content hashes
/// so a later incremental semantic pass does not skip structurally-current but
/// semantically-stale files.
fn migrate_0003_semantic_files(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS semantic_files (
             chunk_table TEXT NOT NULL,
             path TEXT NOT NULL,
             content_hash TEXT NOT NULL,
             indexed_at INTEGER NOT NULL DEFAULT (unixepoch()),
             PRIMARY KEY (chunk_table, path)
         );",
    )?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_semantic_files_path ON semantic_files(path);",
    )?;
    Ok(())
}

/// `semantic_files` was introduced as path-only state during the 0.4.6
/// development cycle. Freshness is actually per chunk table because each
/// embedding model has its own vec0 table, so upgrade any path-only table by
/// discarding freshness rows and forcing the next semantic pass to re-index.
fn migrate_0004_semantic_files_chunk_table(conn: &Connection) -> rusqlite::Result<()> {
    let has_chunk_table: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('semantic_files') WHERE name = 'chunk_table'",
        [],
        |row| row.get(0),
    )?;

    if has_chunk_table == 0 {
        conn.execute_batch(
            "DROP TABLE IF EXISTS semantic_files_path_only_backup;
             ALTER TABLE semantic_files RENAME TO semantic_files_path_only_backup;
             CREATE TABLE semantic_files (
                 chunk_table TEXT NOT NULL,
                 path TEXT NOT NULL,
                 content_hash TEXT NOT NULL,
                 indexed_at INTEGER NOT NULL DEFAULT (unixepoch()),
                 PRIMARY KEY (chunk_table, path)
             );
             DROP TABLE semantic_files_path_only_backup;
             CREATE INDEX IF NOT EXISTS idx_semantic_files_path ON semantic_files(path);",
        )?;
    } else {
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_semantic_files_path ON semantic_files(path);",
        )?;
    }
    Ok(())
}

/// Records the exact original model name and dimension for each vec0 chunk
/// table. Chunk table names are sanitized for SQLite identifiers, so the exact
/// metadata is the authoritative lookup key for no-embedder contexts.
fn migrate_0005_semantic_models(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS semantic_models (
             chunk_table TEXT PRIMARY KEY,
             model TEXT NOT NULL,
             dim INTEGER NOT NULL,
             indexed_at INTEGER NOT NULL DEFAULT (unixepoch())
         );
         CREATE INDEX IF NOT EXISTS idx_semantic_models_model ON semantic_models(model);",
    )?;
    Ok(())
}

pub fn ensure_chunk_table(conn: &Connection, table_name: &str, dim: usize) -> rusqlite::Result<()> {
    conn.execute_batch(&semantic_schema(table_name, dim))?;
    let fts = fts_table_name(table_name);
    conn.execute_batch(&fts_schema(&fts))?;
    repair_fts_sidecar(conn, table_name, &fts)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_initialized() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        init_db(&conn).expect("init_db");
        conn
    }

    fn pragma_string(conn: &Connection, name: &str) -> String {
        conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get::<_, String>(0))
            .unwrap_or_else(|e| panic!("PRAGMA {name}: {e}"))
    }

    fn pragma_int(conn: &Connection, name: &str) -> i64 {
        conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get::<_, i64>(0))
            .unwrap_or_else(|e| panic!("PRAGMA {name}: {e}"))
    }

    #[test]
    fn init_db_sets_wal_journal_mode() {
        let conn = open_initialized();
        // In-memory DBs report "memory" not "wal" — only file-backed DBs
        // honor journal_mode=WAL. Verify the file-backed path separately.
        let mode = pragma_string(&conn, "journal_mode");
        assert_eq!(mode, "memory", "in-memory db journal_mode is 'memory'");
    }

    #[test]
    fn init_db_sets_wal_on_file_backed_db() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("idx.db");
        let conn = Connection::open(&path).expect("open file db");
        init_db(&conn).expect("init_db");
        let mode = pragma_string(&conn, "journal_mode");
        assert_eq!(mode.to_lowercase(), "wal", "expected WAL journal mode");
    }

    #[test]
    fn init_db_sets_foreign_keys_on() {
        let conn = open_initialized();
        assert_eq!(pragma_int(&conn, "foreign_keys"), 1);
    }

    #[test]
    fn init_db_sets_busy_timeout() {
        // Repowise alignment (sweep §1.8): a non-zero busy_timeout means a
        // second MCP session reading mid-index waits briefly instead of
        // failing instantly with SQLITE_BUSY. Default is 0.
        let conn = open_initialized();
        let timeout_ms = pragma_int(&conn, "busy_timeout");
        assert!(
            timeout_ms >= 5000,
            "expected busy_timeout >= 5000ms, got {timeout_ms}",
        );
    }
}
