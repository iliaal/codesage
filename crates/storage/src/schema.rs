use std::sync::Once;

use rusqlite::Connection;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS files (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    language TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    indexed_at INTEGER NOT NULL DEFAULT (unixepoch())
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
    col_end INTEGER NOT NULL
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

pub fn model_table_name(model: &str, dim: usize) -> String {
    let sanitized: String = model
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    format!("chunks_{sanitized}_{dim}")
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
    VEC_INIT.call_once(|| unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(sqlite3_vec_init));
    });
}

pub fn init_db(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;
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

const MIGRATIONS: &[(&str, MigrationUp)] = &[("0001_refs_name_tail", migrate_0001_refs_name_tail)];

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

/// Extract the trailing segment of a qualified name past the last `\`, `/`, or `::`.
/// PHP `App\Http\Controllers\Foo` → `Foo`; Rust `mod::sub::bar` → `bar`; path `a/b/c` → `c`.
pub fn name_tail(s: &str) -> &str {
    let mut best: Option<usize> = None;
    if let Some(p) = s.rfind('\\') {
        best = Some(p + 1);
    }
    if let Some(p) = s.rfind('/') {
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

pub fn ensure_chunk_table(conn: &Connection, table_name: &str, dim: usize) -> rusqlite::Result<()> {
    conn.execute_batch(&semantic_schema(table_name, dim))
}
