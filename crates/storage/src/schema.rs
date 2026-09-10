use std::sync::Once;

use rusqlite::Connection;

pub(crate) const SCHEMA: &str = r#"
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

-- One MinHash fingerprint per function/method definition, for near-clone
-- (SIMILAR_TO) detection. `fp` is 64 little-endian u64 (512 bytes). Rebuilt
-- per file on reindex, same lifecycle as `symbols`.
CREATE TABLE IF NOT EXISTS symbol_fingerprints (
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    line_start INTEGER NOT NULL,
    line_end INTEGER NOT NULL,
    leaf_count INTEGER NOT NULL,
    fp BLOB NOT NULL CHECK (length(fp) = 512)
);

CREATE INDEX IF NOT EXISTS idx_symfp_file ON symbol_fingerprints(file_id);
CREATE INDEX IF NOT EXISTS idx_symfp_name ON symbol_fingerprints(name);

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

-- Covers `ORDER BY churn_score DESC, path LIMIT ?` as a streaming scan. The
-- `path` term is what makes the top-N deterministic when churn scores tie
-- (common: every file with one commit at the same time decays identically);
-- carrying it in the index keeps SQLite from falling back to a full scan plus
-- a temp b-tree sorter to satisfy it.
CREATE INDEX IF NOT EXISTS idx_git_files_churn ON git_files(churn_score DESC, path);

CREATE TABLE IF NOT EXISTS git_co_changes (
    file_a TEXT NOT NULL,
    file_b TEXT NOT NULL,
    weight REAL NOT NULL DEFAULT 0,
    count INTEGER NOT NULL DEFAULT 0,
    last_observed_at INTEGER,
    first_observed_at INTEGER,
    window_mask INTEGER NOT NULL DEFAULT 0,
    windows INTEGER NOT NULL DEFAULT 1,
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
    indexed_at INTEGER NOT NULL DEFAULT (unixepoch()),
    fingerprint TEXT,
    artifact_digest TEXT,
    artifact_stat_key TEXT
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

pub(crate) fn semantic_schema(table_name: &str, dim: usize) -> String {
    // Legacy registry names need quoting even when generated names are sanitized.
    let table = quote_ident(table_name);
    format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS \"{table}\" USING vec0(\
         id INTEGER PRIMARY KEY, \
         +file_path TEXT, \
         language TEXT partition key, \
         +content TEXT, \
         +start_line INTEGER, \
         +end_line INTEGER, \
         embedding float[{dim}]);"
    )
}

pub fn fts_table_name(chunk_table: &str) -> String {
    format!("{chunk_table}_fts")
}

/// DDL for the FTS5 sidecar. `tokenchars '_'` keeps identifiers like
/// `doc_cfg`, `mb_convert_case`, `moduleref` intact instead of splitting
/// them into half-useful tokens. No Porter stemmer — we match code
/// identifiers verbatim, not English.
pub(crate) fn fts_schema(table_name: &str) -> String {
    let table = quote_ident(table_name);
    format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS \"{table}\" USING fts5(\
         content, \
         file_path UNINDEXED, \
         language UNINDEXED, \
         start_line UNINDEXED, \
         end_line UNINDEXED, \
         tokenize = \"unicode61 remove_diacritics 1 tokenchars '_'\");"
    )
}

pub(crate) fn fts_vocab_schema(fts_table_name: &str) -> String {
    let vocab_table = format!("{fts_table_name}_vocab");
    format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS \"{}\" USING fts5vocab(\"{}\", row);",
        quote_ident(&vocab_table),
        quote_ident(fts_table_name)
    )
}

pub(crate) fn quote_ident(identifier: &str) -> String {
    identifier.replace('"', "\"\"")
}

fn table_row_count(conn: &Connection, table_name: &str) -> rusqlite::Result<i64> {
    let sql = format!("SELECT COUNT(*) FROM \"{}\"", quote_ident(table_name));
    conn.query_row(&sql, [], |row| row.get(0))
}

fn table_max_id(conn: &Connection, table_name: &str, id_col: &str) -> rusqlite::Result<i64> {
    let sql = format!(
        "SELECT COALESCE(MAX(\"{}\"), 0) FROM \"{}\"",
        quote_ident(id_col),
        quote_ident(table_name)
    );
    conn.query_row(&sql, [], |row| row.get(0))
}

/// Whether the FTS5 sidecar mirrors its chunk table. Returned by the
/// read-path health probe ([`fts_sidecar_health`]) so diagnostics can report
/// divergence without paying for a repair, and by the capped repair when the
/// table is over the row cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FtsSidecarHealth {
    /// Count + MAX(rowid) match: the sidecar mirrors the chunk table.
    InSync,
    /// The sidecar diverges; `chunk_rows` / `fts_rows` are the two counts so
    /// a diagnostic can say how far apart they are.
    Diverged { chunk_rows: i64, fts_rows: i64 },
}

/// What a bounded FTS repair did. [`FtsRepairOutcome::SkippedOverCap`] is the
/// health signal for a diverged table too large to rebuild synchronously on
/// open: the caller logs it and carries on with a degraded BM25 sidecar
/// rather than stalling startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FtsRepairOutcome {
    /// The sidecar already mirrored the chunk table; nothing was rewritten.
    InSync,
    /// The sidecar was rebuilt from the chunk table; `rows` is the chunk
    /// count that was copied.
    Repaired { rows: i64 },
    /// The sidecar diverges but the chunk table holds more than `max_rows`
    /// rows, so no rebuild ran. Repair explicitly off the open path (see
    /// [`Database::repair_fts_sidecar`]).
    SkippedOverCap { chunk_rows: i64 },
}

/// Cap synchronous FTS rewrites at open; larger tables require explicit repair.
pub const FTS_REPAIR_ROW_CAP: i64 = 100_000;

fn fts_sidecar_counts(
    conn: &Connection,
    chunk_table: &str,
    fts_table: &str,
) -> rusqlite::Result<(i64, i64, bool)> {
    let chunk_count = table_row_count(conn, chunk_table)?;
    let fts_count = table_row_count(conn, fts_table)?;
    // MAX(rowid) also catches equal-count delete/insert drift; this is no checksum.
    let in_sync = chunk_count == fts_count
        && table_max_id(conn, chunk_table, "id")? == table_max_id(conn, fts_table, "rowid")?;
    Ok((chunk_count, fts_count, in_sync))
}

/// Read-path probe: does the FTS5 sidecar mirror `chunk_table`? Two cheap
/// queries, never a rewrite — safe on every open, including read-only and
/// migration-free handles.
pub(crate) fn fts_sidecar_health(
    conn: &Connection,
    chunk_table: &str,
    fts_table: &str,
) -> rusqlite::Result<FtsSidecarHealth> {
    let (chunk_count, fts_count, in_sync) = fts_sidecar_counts(conn, chunk_table, fts_table)?;
    Ok(if in_sync {
        FtsSidecarHealth::InSync
    } else {
        FtsSidecarHealth::Diverged {
            chunk_rows: chunk_count,
            fts_rows: fts_count,
        }
    })
}

pub(crate) fn repair_fts_sidecar(
    conn: &Connection,
    chunk_table: &str,
    fts_table: &str,
) -> rusqlite::Result<()> {
    let chunk_table = quote_ident(chunk_table);
    let fts_table = quote_ident(fts_table);
    // Keep DELETE/repopulate atomic; savepoints compose with outer transactions.
    let sql = format!(
        "SAVEPOINT repair_fts;
         DELETE FROM \"{fts_table}\";
         INSERT INTO \"{fts_table}\"(rowid, content, file_path, language, start_line, end_line)
         SELECT id, content, file_path, language, start_line, end_line
         FROM \"{chunk_table}\"
         ORDER BY id;
         RELEASE repair_fts;"
    );
    if let Err(e) = conn.execute_batch(&sql) {
        let _ = conn.execute_batch("ROLLBACK TO repair_fts");
        let _ = conn.execute_batch("RELEASE repair_fts");
        return Err(e);
    }
    Ok(())
}

/// Bounded repair for open paths: rebuilds the sidecar when it diverges and
/// the chunk table holds at most `max_rows` rows, otherwise reports
/// [`FtsRepairOutcome::SkippedOverCap`] without rewriting anything. Open
/// paths pass [`FTS_REPAIR_ROW_CAP`]; tests pass a small cap to exercise the
/// skip without a 100 k-row fixture.
pub(crate) fn repair_fts_sidecar_capped(
    conn: &Connection,
    chunk_table: &str,
    fts_table: &str,
    max_rows: i64,
) -> rusqlite::Result<FtsRepairOutcome> {
    let (chunk_count, fts_count, in_sync) = fts_sidecar_counts(conn, chunk_table, fts_table)?;
    if in_sync {
        return Ok(FtsRepairOutcome::InSync);
    }
    if chunk_count > max_rows {
        return Ok(FtsRepairOutcome::SkippedOverCap {
            chunk_rows: chunk_count,
        });
    }
    repair_fts_sidecar(conn, chunk_table, fts_table)?;
    let _ = fts_count;
    Ok(FtsRepairOutcome::Repaired { rows: chunk_count })
}

pub fn model_table_name(model: &str, dim: usize) -> String {
    format!("{}{dim}", model_table_prefix(model))
}

pub(crate) fn model_table_prefix(model: &str) -> String {
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

pub(crate) fn init_vec_extension() {
    // SAFETY: sqlite-vec exposes a valid SQLite extension entrypoint with the
    // signature required by `sqlite3_auto_extension`, and registration is
    // process-global/idempotent behind `Once`.
    VEC_INIT.call_once(|| unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(sqlite3_vec_init));
    });
}

/// Readers tolerate longer indexing transactions than the 5-second writer timeout.
/// SQLite retries internally until this bound, then returns SQLITE_BUSY.
pub(crate) const READ_BUSY_TIMEOUT_MS: i64 = 30_000;

pub(crate) fn set_busy_timeout(conn: &Connection, ordinary_ms: i64) -> rusqlite::Result<()> {
    let millis = if codesage_protocol::work::current().is_some() {
        100
    } else {
        ordinary_ms as u64
    };
    conn.busy_timeout(std::time::Duration::from_millis(millis))
}

/// Connection-local read pragmas; init_db also changes journal mode and migrates.
pub fn init_db_read_only(conn: &Connection) -> rusqlite::Result<()> {
    set_busy_timeout(conn, READ_BUSY_TIMEOUT_MS)?;
    conn.execute_batch("PRAGMA mmap_size=268435456;")?;
    conn.execute_batch("PRAGMA cache_size=-65536;")?;
    Ok(())
}

pub fn init_db(conn: &Connection) -> rusqlite::Result<()> {
    // Set the timeout before WAL's exclusive-lock attempt to tolerate brief contention.
    set_busy_timeout(conn, 5000)?;
    // The index is rebuildable; retain the current mode if contention outlasts
    // the timeout, and let the next opener retry WAL.
    if let Err(e) = conn.execute_batch("PRAGMA journal_mode=WAL;") {
        tracing::warn!(
            error = %e,
            "could not switch index to WAL journal mode; continuing in the current mode"
        );
    }
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;
    // WAL/NORMAL may lose commits since the last checkpoint on power loss, but
    // this derived index is rebuildable. Negative cache_size is KiB (64 MiB);
    // mmap uses the OS page cache without pinning RSS.
    conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
    conn.execute_batch("PRAGMA mmap_size=268435456;")?;
    conn.execute_batch("PRAGMA cache_size=-65536;")?;
    conn.execute_batch(SCHEMA)?;
    run_migrations(conn)?;
    Ok(())
}

/// Migration bodies must be safe on an already-current schema: init_db creates
/// SCHEMA before running the registry. Check target state and no-op if present.
/// The runner atomically commits each body with its stamp; do not nest transactions.
type MigrationUp = fn(&Connection) -> rusqlite::Result<()>;

/// Reserved name prefix for destructive migrations. A future migration that
/// rewrites data in a way older binaries cannot safely read past must be
/// named `breaking_<nnnn>_<desc>`; any binary that finds such a row in
/// `schema_migrations` without knowing the migration itself refuses to open
/// the DB. Unknown migrations WITHOUT this prefix are treated as additive:
/// the open proceeds with a loud warning so mixed-version setups keep
/// working.
pub const BREAKING_MIGRATION_PREFIX: &str = "breaking_";

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
    ("0012_symbol_fingerprints", migrate_0012_symbol_fingerprints),
    (
        "0013_structural_unique_keys",
        migrate_0013_structural_unique_keys,
    ),
    (
        "0014_git_files_churn_path",
        migrate_0014_git_files_churn_path,
    ),
    (
        "0015_semantic_models_fingerprint",
        migrate_0015_semantic_models_fingerprint,
    ),
    (
        "0016_semantic_models_artifact_stat_key",
        migrate_0016_semantic_models_artifact_stat_key,
    ),
    (
        "0017_git_co_changes_recurrence",
        migrate_0017_git_co_changes_recurrence,
    ),
    ("0018_git_author_events", migrate_0018_git_author_events),
    ("0019_file_hash_cache", migrate_0019_file_hash_cache),
];

fn migrate_0019_file_hash_cache(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS file_hash_cache (
        path TEXT PRIMARY KEY,
        size INTEGER NOT NULL,
        mtime_ns INTEGER NOT NULL,
        ctime_ns INTEGER NOT NULL,
        content_hash TEXT NOT NULL,
        hashed_at_ns INTEGER NOT NULL
    );",
    )
}

fn migrate_0018_git_author_events(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS git_author_events (
            file_path TEXT NOT NULL,
            commit_sha TEXT NOT NULL,
            author TEXT NOT NULL,
            committed_at INTEGER NOT NULL,
            PRIMARY KEY (file_path, commit_sha)
        );
        CREATE TABLE IF NOT EXISTS git_author_state (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            complete INTEGER NOT NULL CHECK (complete IN (0, 1))
        );",
    )
}

/// Co-change recurrence columns on `git_co_changes`: `first_observed_at`
/// (oldest shared commit), `window_mask` (bit `(ts / 90d) % 64` per shared
/// commit, keyed to the unix epoch so incremental ORs compose exactly with a
/// full rescan), and `windows` (`popcount(window_mask)`, kept as a column so
/// SQL can order by it). Rows written before the columns existed read
/// `NULL / 0 / 1` until the next `codesage git-index --full` recomputes them.
fn migrate_0017_git_co_changes_recurrence(conn: &Connection) -> rusqlite::Result<()> {
    for (column, decl) in [
        ("first_observed_at", "INTEGER"),
        ("window_mask", "INTEGER NOT NULL DEFAULT 0"),
        ("windows", "INTEGER NOT NULL DEFAULT 1"),
    ] {
        let has_column: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('git_co_changes') WHERE name = ?1",
            rusqlite::params![column],
            |row| row.get(0),
        )?;
        if has_column == 0 {
            conn.execute_batch(&format!(
                "ALTER TABLE git_co_changes ADD COLUMN {column} {decl};"
            ))?;
        }
    }
    Ok(())
}

/// `semantic_models.fingerprint`: the embedding setup (model, pinned files,
/// dimension, pooling, chunker) whose vectors the chunk table holds. NULL on a
/// table populated before the column existed, which readers treat as
/// "unknown" — stored vectors are not reused until a full rebuild records it.
fn migrate_0015_semantic_models_fingerprint(conn: &Connection) -> rusqlite::Result<()> {
    let has_column: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('semantic_models') WHERE name = 'fingerprint'",
        [],
        |row| row.get(0),
    )?;
    if has_column == 0 {
        conn.execute_batch("ALTER TABLE semantic_models ADD COLUMN fingerprint TEXT;")?;
    }
    Ok(())
}

/// `semantic_models.artifact_digest` and `artifact_stat_key`: the model-file
/// digest the recorded fingerprint was built over and the path/size/mtime key
/// of those files at the time. A later process compares the stat key first
/// and re-reads the model files only when it differs. NULL on a row attested
/// before the columns existed, which forces one read.
fn migrate_0016_semantic_models_artifact_stat_key(conn: &Connection) -> rusqlite::Result<()> {
    for column in ["artifact_digest", "artifact_stat_key"] {
        let has_column: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('semantic_models') WHERE name = ?1",
            rusqlite::params![column],
            |row| row.get(0),
        )?;
        if has_column == 0 {
            conn.execute_batch(&format!(
                "ALTER TABLE semantic_models ADD COLUMN {column} TEXT;"
            ))?;
        }
    }
    Ok(())
}

/// Include path tie-breaking in the index to avoid a temp sorter before LIMIT.
fn migrate_0014_git_files_churn_path(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "DROP INDEX IF EXISTS idx_git_files_churn;
         CREATE INDEX IF NOT EXISTS idx_git_files_churn
             ON git_files(churn_score DESC, path);",
    )
}

fn run_migrations(conn: &Connection) -> rusqlite::Result<()> {
    run_migration_list(conn, MIGRATIONS)
}

fn run_migration_list(
    conn: &Connection,
    migrations: &[(&str, MigrationUp)],
) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
             id INTEGER PRIMARY KEY,
             name TEXT NOT NULL UNIQUE,
             applied_at INTEGER NOT NULL DEFAULT (unixepoch())
         );",
    )?;
    // BEGIN IMMEDIATE serializes migrations before their first write.
    // Writer commands also hold indexing.lock; lock-bypass opens rely on
    // init_db's 5-second busy timeout and may still fail after that window.
    for (name, up) in migrations {
        let already: i64 = conn.query_row(
            "SELECT COUNT(*) FROM schema_migrations WHERE name = ?1",
            rusqlite::params![name],
            |r| r.get(0),
        )?;
        if already > 0 {
            continue;
        }
        conn.execute_batch("BEGIN IMMEDIATE")?;
        // Another opener may have stamped this migration since the unlocked probe.
        let stamped_meanwhile: i64 = match conn.query_row(
            "SELECT COUNT(*) FROM schema_migrations WHERE name = ?1",
            rusqlite::params![name],
            |r| r.get(0),
        ) {
            Ok(n) => n,
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e);
            }
        };
        if stamped_meanwhile > 0 {
            conn.execute_batch("ROLLBACK")?;
            continue;
        }
        if let Err(e) = (|| -> rusqlite::Result<()> {
            up(conn)?;
            conn.execute(
                "INSERT OR IGNORE INTO schema_migrations (name) VALUES (?1)",
                rusqlite::params![name],
            )?;
            Ok(())
        })() {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(e);
        }
        if let Err(e) = conn.execute_batch("COMMIT") {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(e);
        }
    }
    forget_superseded_migrations(conn)?;
    check_unknown_migrations(conn)?;
    Ok(())
}

/// Development-build names replaced by idempotent migrations in the current list.
const SUPERSEDED_MIGRATIONS: &[&str] = &["0017_git_co_changes_windows"];

fn forget_superseded_migrations(conn: &Connection) -> rusqlite::Result<()> {
    for name in SUPERSEDED_MIGRATIONS {
        conn.execute(
            "DELETE FROM schema_migrations WHERE name = ?1",
            rusqlite::params![name],
        )?;
    }
    Ok(())
}

/// Detect `schema_migrations` rows this binary doesn't know about — i.e. the
/// DB was migrated by a newer codesage. Additive unknowns warn; a
/// [`BREAKING_MIGRATION_PREFIX`] row hard-errors, because that name is the
/// newer binary's declaration that older code must not proceed.
fn check_unknown_migrations(conn: &Connection) -> rusqlite::Result<()> {
    let known: std::collections::HashSet<&str> = MIGRATIONS.iter().map(|(name, _)| *name).collect();
    let unknown: Vec<String> = {
        let mut stmt = conn.prepare("SELECT name FROM schema_migrations ORDER BY name")?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .filter(|name| !known.contains(name.as_str()))
            .collect()
    };
    if let Some(breaking) = unknown
        .iter()
        .find(|name| name.starts_with(BREAKING_MIGRATION_PREFIX))
    {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
            Some(format!(
                "index was migrated by a newer codesage (breaking migration {breaking:?}); \
                 this binary is too old to open it safely — upgrade codesage or rebuild the index"
            )),
        ));
    }
    if !unknown.is_empty() {
        tracing::warn!(
            migrations = ?unknown,
            "schema_migrations has entries unknown to this binary — the index was \
             migrated by a newer codesage; proceeding (additive migrations only)"
        );
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

/// Rows per backfill page in [`backfill_refs_name_tail`]. Bounds the
/// migration's heap to one page of `(id, to_name)` pairs instead of the
/// whole `refs` table; 500 rows of short strings is tens of KiB.
const NAME_TAIL_BACKFILL_CHUNK_ROWS: i64 = 500;

/// Recompute `refs.to_name_tail` for every row, one id-ordered page at a
/// time. `name_tail` splits past the last `\`, `/`, `.`, or two-char `::`,
/// which has no faithful pure-SQL spelling in SQLite's string functions —
/// hence a Rust-side cursor rather than a set-based UPDATE. Paginating by
/// `id > last_seen` (not OFFSET) keeps each page a bounded indexed range
/// scan and stays correct while the UPDATEs themselves change no ids.
fn backfill_refs_name_tail(conn: &Connection) -> rusqlite::Result<()> {
    let mut last_id: i64 = 0;
    loop {
        let rows: Vec<(i64, String)> = {
            let mut stmt =
                conn.prepare("SELECT id, to_name FROM refs WHERE id > ?1 ORDER BY id LIMIT ?2")?;
            stmt.query_map(
                rusqlite::params![last_id, NAME_TAIL_BACKFILL_CHUNK_ROWS],
                |row| Ok((row.get(0)?, row.get::<_, String>(1)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        if rows.is_empty() {
            break;
        }
        {
            let mut update = conn.prepare("UPDATE refs SET to_name_tail = ?1 WHERE id = ?2")?;
            for (id, to_name) in &rows {
                update.execute(rusqlite::params![name_tail(to_name), id])?;
                last_id = last_id.max(*id);
            }
        }
        if (rows.len() as i64) < NAME_TAIL_BACKFILL_CHUNK_ROWS {
            break;
        }
    }
    Ok(())
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
        backfill_refs_name_tail(conn)?;
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

/// Keep mutable test commands outside entry_command, which contributes to feature
/// identity. Legacy NULLs populate on the next mapping pass.
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

/// Deduplicate before adding UNIQUE indexes; never add them to base SCHEMA,
/// which runs before this cleanup on legacy databases. Lowest id/rowid wins.
/// GROUP BY avoids correlated-EXISTS quadratic probes on hot names.
/// Exclude synthetic route_handler rows: same-line registrations share col=0
/// and the mapper replaces them by kind, outside parser row identity.
/// Additive: older writers already use delete-then-insert.
fn migrate_0013_structural_unique_keys(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "DELETE FROM symbols WHERE id NOT IN (
             SELECT MIN(id) FROM symbols
             GROUP BY file_id, name, qualified_name, kind,
                      line_start, line_end, col_start, col_end
         );
         DELETE FROM refs WHERE kind <> 'route_handler' AND id NOT IN (
             SELECT MIN(id) FROM refs WHERE kind <> 'route_handler'
             GROUP BY from_file_id, to_name, kind, line, col
         );
         DELETE FROM symbol_fingerprints WHERE rowid NOT IN (
             SELECT MIN(rowid) FROM symbol_fingerprints
             GROUP BY file_id, name, kind, line_start, line_end
         );
         CREATE UNIQUE INDEX IF NOT EXISTS uq_symbols_identity
             ON symbols(file_id, name, qualified_name, kind,
                        line_start, line_end, col_start, col_end);
         CREATE UNIQUE INDEX IF NOT EXISTS uq_refs_identity
             ON refs(from_file_id, to_name, kind, line, col)
             WHERE kind <> 'route_handler';
         CREATE UNIQUE INDEX IF NOT EXISTS uq_symfp_identity
             ON symbol_fingerprints(file_id, name, kind, line_start, line_end);",
    )?;
    Ok(())
}

/// Adds the `symbol_fingerprints` table (per-function MinHash for near-clone
/// detection). Idempotent via `IF NOT EXISTS`; existing indexes get the table
/// empty and populate it on the next `codesage index`. `fp` is a fixed
/// 512-byte BLOB (64 little-endian u64).
fn migrate_0012_symbol_fingerprints(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS symbol_fingerprints (
             file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
             name TEXT NOT NULL,
             kind TEXT NOT NULL,
             line_start INTEGER NOT NULL,
             line_end INTEGER NOT NULL,
             leaf_count INTEGER NOT NULL,
             fp BLOB NOT NULL CHECK (length(fp) = 512)
         );
         CREATE INDEX IF NOT EXISTS idx_symfp_file ON symbol_fingerprints(file_id);
         CREATE INDEX IF NOT EXISTS idx_symfp_name ON symbol_fingerprints(name);",
    )?;
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

/// Boundary rows populate on reindex/rederivation, not during migration;
/// files without rows contribute zero trust-boundary risk until then.
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

/// Recompute `to_name_tail` under the dotted-tail rule (0001 only split on
/// `\`, `/`, `::`; the dot matters for Go `fmt.Println`-style names).
/// No-op when the column does not exist yet — 0001 runs first in the same
/// `run_migrations` pass and backfills on column creation. Paged through
/// [`backfill_refs_name_tail`] so a php-src-scale `refs` table is never
/// materialized into one `Vec`.
fn migrate_0006_refs_name_tail_dot(conn: &Connection) -> rusqlite::Result<()> {
    let has_column: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('refs') WHERE name = 'to_name_tail'",
        [],
        |row| row.get(0),
    )?;
    if has_column == 0 {
        return Ok(());
    }
    backfill_refs_name_tail(conn)
}

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

/// Replace legacy path-only freshness with per-model-table state; discarding
/// old stamps forces semantic reindexing instead of reusing another model's hash.
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

/// DDL only: create missing vec0/FTS/vocab tables without a potentially large rebuild.
/// Write paths use capped repair; read paths inspect health without rewriting.
pub(crate) fn ensure_chunk_table(
    conn: &Connection,
    table_name: &str,
    dim: usize,
) -> rusqlite::Result<()> {
    conn.execute_batch(&semantic_schema(table_name, dim))?;
    let fts = fts_table_name(table_name);
    conn.execute_batch(&fts_schema(&fts))?;
    conn.execute_batch(&fts_vocab_schema(&fts))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A superseded name that is also a live migration would be deleted and
    /// re-applied on every open; the two lists must stay disjoint.
    #[test]
    fn superseded_migrations_are_not_live_migrations() {
        for superseded in SUPERSEDED_MIGRATIONS {
            assert!(
                MIGRATIONS.iter().all(|(name, _)| name != superseded),
                "{superseded} is listed both as superseded and as a live migration"
            );
        }
    }

    const RACE_NAME: &str = "9998_race_probe";

    /// A migration whose body stamps its own name, standing in for a racing
    /// opener that committed the stamp while this one was running. A plain
    /// `INSERT` stamp after it violates the UNIQUE name; `OR IGNORE` does not.
    fn up_stamps_itself(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute(
            "INSERT INTO schema_migrations (name) VALUES (?1)",
            rusqlite::params![RACE_NAME],
        )?;
        Ok(())
    }

    #[test]
    fn migration_stamp_already_written_inside_the_transaction_does_not_fail() {
        let conn = open_initialized();
        run_migration_list(&conn, &[(RACE_NAME, up_stamps_itself)])
            .expect("stamp written during `up` must be tolerated");
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE name = ?1",
                rusqlite::params![RACE_NAME],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    /// The second connection that wins the race. Stashed in a static because
    /// `busy_handler` takes a plain `fn` pointer.
    static RACER: std::sync::Mutex<Option<Connection>> = std::sync::Mutex::new(None);

    /// Invoked when the migrating connection's `BEGIN IMMEDIATE` finds the
    /// racer holding the write lock: the racer stamps the migration and
    /// commits, then the migrator retries and must see the stamp under its
    /// own lock. Returns `true` (retry) only for that first invocation; a
    /// second busy signal means the lock is held by something else and the
    /// test should fail fast rather than spin.
    fn racer_stamps_and_releases(_attempt: i32) -> bool {
        let mut guard = RACER.lock().unwrap();
        match guard.take() {
            Some(racer) => {
                racer
                    .execute(
                        "INSERT INTO schema_migrations (name) VALUES (?1)",
                        rusqlite::params![RACE_NAME],
                    )
                    .unwrap();
                racer.execute_batch("COMMIT").unwrap();
                true
            }
            None => false,
        }
    }

    fn up_must_not_run(_conn: &Connection) -> rusqlite::Result<()> {
        Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_MISUSE),
            Some("migration ran although a racing opener had stamped it".to_string()),
        ))
    }

    /// Stamp lands between the unlocked probe and the write lock: the
    /// re-probe under `BEGIN IMMEDIATE` must skip the migration. Without the
    /// re-probe the body runs a second time (here: errors on purpose).
    #[test]
    fn migration_stamped_by_a_racing_opener_between_probe_and_lock_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("race.db");
        let conn = Connection::open(&path).unwrap();
        init_db(&conn).unwrap();

        let racer = Connection::open(&path).unwrap();
        racer.execute_batch("BEGIN IMMEDIATE").unwrap();
        *RACER.lock().unwrap() = Some(racer);
        conn.busy_handler(Some(racer_stamps_and_releases)).unwrap();

        run_migration_list(&conn, &[(RACE_NAME, up_must_not_run)])
            .expect("racing stamp must be seen under the lock and skipped");
        assert!(
            RACER.lock().unwrap().is_none(),
            "the busy handler must have fired (BEGIN IMMEDIATE was blocked)"
        );
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE name = ?1",
                rusqlite::params![RACE_NAME],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

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

    /// A reader holding an open transaction on a database still in rollback
    /// mode blocks the switch into WAL. The busy timeout must already be set so
    /// init_db waits for the reader instead of failing at once and leaving the
    /// database in `delete` mode.
    #[test]
    fn init_db_waits_for_a_reader_before_switching_to_wal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fresh.db");
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let reader_path = path.clone();
        let reader = std::thread::spawn(move || {
            let reader = Connection::open(&reader_path).expect("open reader");
            assert_eq!(
                pragma_string(&reader, "journal_mode").to_lowercase(),
                "delete",
                "fresh database must start in rollback mode"
            );
            reader.execute_batch("BEGIN").expect("begin read txn");
            let _: i64 = reader
                .query_row("SELECT COUNT(*) FROM sqlite_master", [], |r| r.get(0))
                .expect("read under the open transaction");
            ready_tx.send(()).expect("signal ready");
            std::thread::sleep(std::time::Duration::from_millis(500));
            reader.execute_batch("COMMIT").expect("end read txn");
        });
        ready_rx.recv().expect("reader ready");

        let conn = Connection::open(&path).expect("open file db");
        // rusqlite installs a 5 s busy timeout on open; clear it so the test
        // pins init_db's own ordering rather than the driver default.
        conn.busy_timeout(std::time::Duration::ZERO)
            .expect("clear driver default busy timeout");
        init_db(&conn).expect("init_db must wait for the reader, not fail on SQLITE_BUSY");
        assert_eq!(
            pragma_string(&conn, "journal_mode").to_lowercase(),
            "wal",
            "the switch must complete once the reader releases its lock"
        );
        reader.join().expect("reader thread");
    }

    #[test]
    fn init_db_sets_foreign_keys_on() {
        let conn = open_initialized();
        assert_eq!(pragma_int(&conn, "foreign_keys"), 1);
    }

    fn open_with_chunk_table(table: &str, dim: usize) -> Connection {
        init_vec_extension();
        let conn = Connection::open_in_memory().expect("open in-memory db");
        init_db(&conn).expect("init_db");
        ensure_chunk_table(&conn, table, dim).expect("ensure_chunk_table");
        conn
    }

    fn insert_chunk_pair(conn: &Connection, table: &str, id: i64, content: &str) {
        // dim=2 → embedding is 8 zero bytes.
        conn.execute(
            &format!(
                "INSERT INTO \"{table}\"(id, file_path, language, content, start_line, end_line, embedding)
                 VALUES (?1, 'a.rs', 'rust', ?2, 1, 2, X'0000000000000000')"
            ),
            rusqlite::params![id, content],
        )
        .expect("insert chunk row");
        conn.execute(
            &format!(
                "INSERT INTO \"{}\"(rowid, content, file_path, language, start_line, end_line)
                 VALUES (?1, ?2, 'a.rs', 'rust', 1, 2)",
                fts_table_name(table)
            ),
            rusqlite::params![id, content],
        )
        .expect("insert fts row");
    }

    #[test]
    fn repair_fts_sidecar_repairs_equal_count_max_rowid_divergence() {
        let table = "chunks_repairtest_2";
        let conn = open_with_chunk_table(table, 2);
        insert_chunk_pair(&conn, table, 1, "fn one");
        insert_chunk_pair(&conn, table, 2, "fn two");

        // Equal counts conceal drift; differing maximum IDs must trigger repair.
        conn.execute(&format!("DELETE FROM \"{table}\" WHERE id = 2"), [])
            .unwrap();
        conn.execute(
            &format!(
                "INSERT INTO \"{table}\"(id, file_path, language, content, start_line, end_line, embedding)
                 VALUES (3, 'a.rs', 'rust', 'fn three', 5, 6, X'0000000000000000')"
            ),
            [],
        )
        .unwrap();

        let fts = fts_table_name(table);
        repair_fts_sidecar(&conn, table, &fts).expect("repair");

        let fts_rowids: Vec<i64> = {
            let mut stmt = conn
                .prepare(&format!("SELECT rowid FROM \"{fts}\" ORDER BY rowid"))
                .unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(fts_rowids, vec![1, 3], "fts must mirror the chunk table");
        let content: String = conn
            .query_row(
                &format!("SELECT content FROM \"{fts}\" WHERE rowid = 3"),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(content, "fn three");
    }

    #[test]
    fn repair_fts_sidecar_still_repairs_count_mismatch() {
        let table = "chunks_repaircount_2";
        let conn = open_with_chunk_table(table, 2);
        insert_chunk_pair(&conn, table, 1, "fn one");
        let fts = fts_table_name(table);
        conn.execute(&format!("DELETE FROM \"{fts}\""), []).unwrap();

        repair_fts_sidecar(&conn, table, &fts).expect("repair");

        let n: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM \"{fts}\""), [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "fts must be rebuilt from the chunk table");
    }

    #[test]
    fn init_db_sets_busy_timeout() {
        let conn = open_initialized();
        let timeout_ms = pragma_int(&conn, "busy_timeout");
        assert!(
            timeout_ms >= 5000,
            "expected busy_timeout >= 5000ms, got {timeout_ms}",
        );
    }

    #[test]
    fn capped_repair_skips_over_cap_and_repairs_under_it() {
        let table = "chunks_repaircap_2";
        let conn = open_with_chunk_table(table, 2);
        insert_chunk_pair(&conn, table, 1, "fn one");
        insert_chunk_pair(&conn, table, 2, "fn two");
        let fts = fts_table_name(table);
        conn.execute(&format!("DELETE FROM \"{fts}\""), []).unwrap();

        assert_eq!(
            repair_fts_sidecar_capped(&conn, table, &fts, 1).unwrap(),
            FtsRepairOutcome::SkippedOverCap { chunk_rows: 2 }
        );
        assert_eq!(
            fts_sidecar_health(&conn, table, &fts).unwrap(),
            FtsSidecarHealth::Diverged {
                chunk_rows: 2,
                fts_rows: 0
            }
        );

        assert_eq!(
            repair_fts_sidecar_capped(&conn, table, &fts, FTS_REPAIR_ROW_CAP).unwrap(),
            FtsRepairOutcome::Repaired { rows: 2 }
        );
        assert_eq!(
            fts_sidecar_health(&conn, table, &fts).unwrap(),
            FtsSidecarHealth::InSync
        );
        assert_eq!(
            repair_fts_sidecar_capped(&conn, table, &fts, 0).unwrap(),
            FtsRepairOutcome::InSync
        );
    }

    #[test]
    fn init_db_read_only_sets_raised_busy_timeout() {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        init_db_read_only(&conn).expect("init_db_read_only");
        let timeout_ms = pragma_int(&conn, "busy_timeout");
        assert_eq!(
            timeout_ms, READ_BUSY_TIMEOUT_MS,
            "read-only opens must carry the raised busy timeout"
        );
        assert!(
            timeout_ms > 5000,
            "readers must wait longer than the 5 s write-path window"
        );
    }

    #[test]
    fn init_db_double_run_does_not_reapply_migrations() {
        let conn = open_initialized();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert!(count > 0, "fresh init_db must record migrations");
        init_db(&conn).expect("second init_db");
        let again: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(again, count, "second init_db must be a registry no-op");
    }

    #[test]
    fn author_event_migration_upgrades_and_preserves_existing_events() {
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();
        conn.execute_batch(
            "DROP TABLE git_author_events;
             DROP TABLE git_author_state;
             DELETE FROM schema_migrations WHERE name = '0018_git_author_events';",
        )
        .unwrap();
        init_db(&conn).unwrap();
        let state: i64 = conn
            .query_row("SELECT COUNT(*) FROM git_author_state", [], |r| r.get(0))
            .unwrap();
        assert_eq!(state, 0);
        conn.execute_batch(
            "INSERT INTO git_author_events VALUES ('src/a.rs', 'abc', 'email:author@example.com', 123);
             INSERT INTO git_author_state VALUES (1, 1);",
        ).unwrap();
        migrate_0018_git_author_events(&conn).unwrap();
        init_db(&conn).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM git_author_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let complete: bool = conn
            .query_row("SELECT complete FROM git_author_state", [], |r| r.get(0))
            .unwrap();
        assert!(complete);
    }

    #[test]
    fn concurrent_init_db_opens_both_succeed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("idx.db");
        // Pre-create so both threads open an existing file.
        {
            let conn = Connection::open(&path).expect("open file db");
            init_db(&conn).expect("seed init_db");
        }
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let conn = Connection::open(&path).expect("open file db");
                    init_db(&conn).expect("concurrent init_db must succeed");
                })
            })
            .collect();
        for h in handles {
            h.join().expect("init_db thread panicked");
        }
    }

    /// The chunked backfill must cover every row past the first page:
    /// 1200 rows (> 2 pages of 500) across all four `name_tail`
    /// delimiters, verified row-by-row against the Rust implementation.
    #[test]
    fn name_tail_backfill_pages_past_first_chunk() {
        let conn = open_initialized();
        conn.execute(
            "INSERT INTO files (id, path, language, content_hash) VALUES (1, 'a.rs', 'rust', 'x')",
            [],
        )
        .unwrap();
        let cases = [
            "App\\Http\\Controllers\\Foo",
            "mod::sub::bar",
            "fmt.Println",
            "a/b/c",
            "plain",
        ];
        {
            // `line` carries the loop index: migration 0013's unique index
            // on (from_file_id, to_name, kind, line, col) rejects 1200
            // rows that differ only in `to_name` cycling over 5 values.
            let mut ins = conn
                .prepare(
                    "INSERT INTO refs (from_file_id, to_name, kind, line, col)
                     VALUES (1, ?1, 'call', ?2, 1)",
                )
                .unwrap();
            for i in 0..1200 {
                ins.execute(rusqlite::params![cases[i % cases.len()], i as i64])
                    .unwrap();
            }
        }
        backfill_refs_name_tail(&conn).expect("chunked backfill");
        let tails: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT to_name_tail FROM refs ORDER BY id")
                .unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(tails.len(), 1200);
        for (i, tail) in tails.iter().enumerate() {
            assert_eq!(
                tail,
                name_tail(cases[i % cases.len()]),
                "row {i} tail mismatch"
            );
        }
    }

    #[test]
    fn chunk_ddl_escapes_quote_in_table_name() {
        let ddl = semantic_schema("chunks_evil\"_2", 2);
        assert!(
            ddl.contains("\"chunks_evil\"\"_2\""),
            "vec0 DDL must double the embedded quote, got: {ddl}"
        );
        let fts = fts_schema("chunks_evil\"_2_fts");
        assert!(
            fts.contains("\"chunks_evil\"\"_2_fts\""),
            "FTS DDL must double the embedded quote, got: {fts}"
        );
        init_vec_extension();
        let conn = Connection::open_in_memory().expect("open in-memory db");
        init_db(&conn).expect("init_db");
        ensure_chunk_table(&conn, "chunks_evil\"_2", 2).expect("ensure_chunk_table");
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'chunks_evil\"_2'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "quoted table must exist under its literal name");
    }
}
