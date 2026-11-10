//! Process-wide path-sidecar read verdicts for callers that open a fresh
//! [`Database`] per request.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::Result;
use rusqlite::{Connection, OpenFlags};

use super::{Database, connection_path_still_matches_open_file};

/// Index files observed at once; the least recently attached is dropped
/// beyond this, closing its observer connection.
const MAX_FILES: usize = 32;

/// Cache of the path-sidecar read verdict (does `<chunk>_paths` name exactly
/// the chunk table's `(id, path)` pairs), keyed by index file and chunk table,
/// so a handle opened per request skips a probe that reads the whole vec0
/// `_auxiliary` shadow table.
///
/// A verdict belongs to one committed state of the file, named by `PRAGMA
/// data_version` on an observer connection that never writes. SQLite changes
/// that value after every commit by any other connection, from any process
/// and any binary version, whatever table the commit touched, so no write
/// path has to cooperate. [`PathSidecarVerdicts::attach`] reads it before the
/// handle opens a read transaction, so every later read on the handle sees
/// that state or a newer one; a verdict the handle reaches is stored only if
/// the value has not moved by then, which proves the handle read that exact
/// state. A positive verdict therefore cannot outlive a commit: after a
/// writer repopulates the chunk table, the next attach finds a new version
/// and the handle probes again.
///
/// A stored verdict reaches a later handle as a seed tied to the version it
/// attached at, not as a verdict of its own: the handle's reads may open
/// their snapshot long after the attach, past a commit. Each use confirms,
/// inside the read snapshot it serves, that the observer still reads that
/// version; a snapshot taken between two equal readings is exactly that
/// state. A seed that fails the check is dropped and the handle probes.
///
/// The observer also holds the file open, so a replacement index cannot
/// reuse its inode while it is cached.
#[derive(Clone, Debug, Default)]
pub struct PathSidecarVerdicts {
    files: Arc<Mutex<Files>>,
}

#[derive(Debug, Default)]
struct Files {
    by_path: HashMap<PathBuf, Observed>,
    next_epoch: u64,
    clock: u64,
}

#[derive(Debug)]
struct Observed {
    observer: Connection,
    identity: FileIdentity,
    epoch: u64,
    touched: u64,
    /// Chunk table → (`data_version` the verdict holds for, readable).
    verdicts: HashMap<String, (i64, bool)>,
}

type FileIdentity = (u64, u64);

/// One handle's attach point: the observer and the version it read then.
#[derive(Debug)]
pub(super) struct Attachment {
    files: Arc<Mutex<Files>>,
    key: PathBuf,
    chunk_table: String,
    epoch: u64,
    version: i64,
}

/// A stored verdict handed to a handle at attach; see [`PathSidecarVerdicts`].
#[derive(Debug)]
pub(super) struct Seed {
    pub(super) readable: bool,
    pub(super) at: Attachment,
}

impl PathSidecarVerdicts {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed `db`'s verdict for its chunk table from the cache, or arrange for
    /// the verdict it reaches to be stored. Call right after opening `db` from
    /// `db_path`, before any read transaction. Best effort: when the file
    /// cannot be attested or observed, the handle probes as it would without
    /// the cache.
    pub fn attach(&self, db: &Database, db_path: &Path) {
        if let Err(error) = self.try_attach(db, db_path) {
            tracing::debug!(
                error = %error,
                path = %db_path.display(),
                "path sidecar verdict cache unavailable for this handle"
            );
        }
    }

    fn try_attach(&self, db: &Database, db_path: &Path) -> Result<()> {
        let chunk_table = db.chunk_table_name();
        if chunk_table.is_empty() || !db.conn.is_autocommit() {
            return Ok(());
        }
        {
            let memo = db.verified_sidecars.borrow();
            if memo.readable_paths.contains_key(chunk_table)
                || memo.verdict_sink.is_some()
                || memo.seed.is_some()
            {
                return Ok(());
            }
        }
        let Some(identity) = file_identity(db_path)? else {
            return Ok(());
        };
        if !connection_path_still_matches_open_file(&db.conn, db_path)? {
            return Ok(());
        }
        let key = std::fs::canonicalize(db_path)?;
        let mut files = lock(&self.files);
        release_stale(&mut files);
        files.clock += 1;
        let clock = files.clock;
        let current = match files.by_path.get(&key) {
            Some(observed) => {
                observed.identity == identity
                    && connection_path_still_matches_open_file(&observed.observer, db_path)?
            }
            None => false,
        };
        if !current {
            files.by_path.remove(&key);
            let observer = open_observer(db_path)?;
            if file_identity(db_path)? != Some(identity)
                || !connection_path_still_matches_open_file(&observer, db_path)?
            {
                return Ok(());
            }
            if files.by_path.len() >= MAX_FILES
                && let Some(oldest) = files
                    .by_path
                    .iter()
                    .min_by_key(|(_, observed)| observed.touched)
                    .map(|(path, _)| path.clone())
            {
                files.by_path.remove(&oldest);
            }
            let epoch = files.next_epoch;
            files.next_epoch += 1;
            files.by_path.insert(
                key.clone(),
                Observed {
                    observer,
                    identity,
                    epoch,
                    touched: clock,
                    verdicts: HashMap::new(),
                },
            );
        }
        let Some(observed) = files.by_path.get_mut(&key) else {
            return Ok(());
        };
        observed.touched = clock;
        let version = data_version(&observed.observer)?;
        let at = Attachment {
            files: Arc::clone(&self.files),
            key,
            chunk_table: chunk_table.to_string(),
            epoch: observed.epoch,
            version,
        };
        let mut memo = db.verified_sidecars.borrow_mut();
        match observed.verdicts.get(chunk_table) {
            Some(&(stored, readable)) if stored == version => {
                memo.seed = Some(Seed { readable, at });
            }
            _ => memo.verdict_sink = Some(at),
        }
        Ok(())
    }
}

impl PathSidecarVerdicts {
    /// Close the observers of index files deleted or replaced at their path,
    /// returning how many were closed. One `stat` per cached file under the
    /// cache lock and no SQL. Attaches prune too, but only a caller that runs
    /// this on a timer releases an index nobody attaches to again; without
    /// one, such an observer lives until the process exits.
    pub fn prune(&self) -> usize {
        release_stale(&mut lock(&self.files))
    }

    /// How many index files have an open observer. Exists only so tests in
    /// dependent crates can see observers released; not a supported surface.
    #[doc(hidden)]
    pub fn observed_files_for_tests(&self) -> usize {
        lock(&self.files).by_path.len()
    }

    /// The verdict stored for `db_path`'s `chunk_table`, whatever version it
    /// holds for. Exists only so tests in dependent crates can see that their
    /// handles were attached; not a supported surface.
    #[doc(hidden)]
    pub fn stored_verdict_for_tests(&self, db_path: &Path, chunk_table: &str) -> Option<bool> {
        let key = std::fs::canonicalize(db_path).ok()?;
        lock(&self.files)
            .by_path
            .get(&key)?
            .verdicts
            .get(chunk_table)
            .map(|&(_, readable)| readable)
    }
}

impl Attachment {
    pub(super) fn chunk_table(&self) -> &str {
        &self.chunk_table
    }

    /// Whether no commit has landed since the handle attached. Fails closed:
    /// an observer that was replaced or cannot be read answers false.
    pub(super) fn unchanged(&self) -> bool {
        let files = lock(&self.files);
        self.unchanged_in(&files)
    }

    fn unchanged_in(&self, files: &Files) -> bool {
        let Some(observed) = files.by_path.get(&self.key) else {
            return false;
        };
        if observed.epoch != self.epoch {
            return false;
        }
        match data_version(&observed.observer) {
            Ok(version) => version == self.version,
            Err(error) => {
                tracing::debug!(error = %error, "could not confirm path sidecar verdict version");
                false
            }
        }
    }

    /// Store `readable` if no commit has landed since the handle attached.
    pub(super) fn offer(&self, chunk_table: &str, readable: bool) {
        if chunk_table != self.chunk_table {
            return;
        }
        let mut files = lock(&self.files);
        if !self.unchanged_in(&files) {
            return;
        }
        if let Some(observed) = files.by_path.get_mut(&self.key) {
            observed
                .verdicts
                .insert(self.chunk_table.clone(), (self.version, readable));
        }
    }
}

/// An index deleted or replaced at its path is never attached again under
/// the identity its observer holds open, so waiting for the next attach to
/// that path could hold the file open indefinitely. A path whose identity
/// cannot be read keeps its observer.
fn release_stale(files: &mut Files) -> usize {
    let before = files.by_path.len();
    files.by_path.retain(|path, observed| {
        !matches!(file_identity(path), Ok(found) if found != Some(observed.identity))
    });
    before - files.by_path.len()
}

fn lock(files: &Mutex<Files>) -> MutexGuard<'_, Files> {
    files.lock().unwrap_or_else(PoisonError::into_inner)
}

fn open_observer(db_path: &Path) -> Result<Connection> {
    let observer = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    observer.busy_timeout(Duration::from_millis(100))?;
    observer.query_row("SELECT count(*) FROM sqlite_master", [], |row| {
        row.get::<_, i64>(0)
    })?;
    Ok(observer)
}

fn data_version(observer: &Connection) -> rusqlite::Result<i64> {
    observer.query_row("PRAGMA data_version", [], |row| row.get(0))
}

#[cfg(unix)]
fn file_identity(path: &Path) -> Result<Option<FileIdentity>> {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(Some((metadata.dev(), metadata.ino()))),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(unix))]
fn file_identity(_path: &Path) -> Result<Option<FileIdentity>> {
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DEFAULT_EMBEDDING_DIM;
    use crate::schema::{path_table_name, quote_ident, semantic_schema};

    const MODEL: &str = "codesage-test/model";

    fn index_two_files(path: &Path) -> String {
        let db = Database::open_for_model(path, MODEL, DEFAULT_EMBEDDING_DIM).unwrap();
        let e = vec![0.5f32; DEFAULT_EMBEDDING_DIM];
        db.insert_chunks(
            "src/a.rs",
            "rust",
            &[
                ("fn a1() {}", 1, 1, e.as_slice()),
                ("fn a2() {}", 2, 2, e.as_slice()),
            ],
        )
        .unwrap();
        db.insert_chunks("src/b.rs", "rust", &[("fn b() {}", 1, 1, e.as_slice())])
            .unwrap();
        db.chunk_table_name().to_string()
    }

    fn open(cache: &PathSidecarVerdicts, path: &Path) -> Database {
        let db = Database::open_read_only_for_existing_model(path, MODEL).unwrap();
        cache.attach(&db, path);
        db
    }

    /// The verdict handed to `db` at attach.
    fn seeded(db: &Database) -> Option<bool> {
        db.verified_sidecars
            .borrow()
            .seed
            .as_ref()
            .map(|seed| seed.readable)
    }

    /// The verdict `db` reached by probing.
    fn remembered(db: &Database) -> Option<bool> {
        db.verified_sidecars
            .borrow()
            .readable_paths
            .get(db.chunk_table_name())
            .map(|read| read.readable)
    }

    /// Swap the paths the sidecar files `src/a.rs` and `src/b.rs` under,
    /// keeping its ids, in a commit from another connection.
    fn swap_sidecar_paths(path: &Path, table: &str) {
        Connection::open(path)
            .unwrap()
            .execute_batch(&format!(
                "UPDATE \"{}\" SET file_path = CASE file_path
                     WHEN 'src/a.rs' THEN 'src/b.rs' ELSE 'src/a.rs' END",
                quote_ident(&path_table_name(table))
            ))
            .unwrap();
    }

    /// What an older binary that never heard of the path sidecar does on a
    /// full rebuild: drop the chunk table and repopulate it in another
    /// order, so the restarted rowids line up with the stale sidecar's ids
    /// while naming other files.
    fn repopulate_without_sidecar(path: &Path, table: &str) {
        let conn = Connection::open(path).unwrap();
        let chunk = quote_ident(table);
        conn.execute_batch(&format!(
            "DROP TABLE \"{chunk}\"; {}",
            semantic_schema(table, DEFAULT_EMBEDDING_DIM)
        ))
        .unwrap();
        let e = crate::embedding_to_bytes(&[0.5f32; DEFAULT_EMBEDDING_DIM]);
        let mut insert = conn
            .prepare(&format!(
                "INSERT INTO \"{chunk}\"(file_path, language, content, start_line, end_line, embedding)
                 VALUES (?1, 'rust', ?2, ?3, ?3, ?4)"
            ))
            .unwrap();
        for (file, content, line) in [
            ("src/b.rs", "fn b() {}", 1),
            ("src/a.rs", "fn a1() {}", 1),
            ("src/a.rs", "fn a2() {}", 2),
        ] {
            insert
                .execute(rusqlite::params![file, content, line, e])
                .unwrap();
        }
    }

    #[test]
    fn attached_handles_share_a_verdict_until_another_connection_commits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let table = index_two_files(&path);
        let cache = PathSidecarVerdicts::new();

        let first = open(&cache, &path);
        assert_eq!(seeded(&first), None);
        assert_eq!(first.chunks_for_file("src/a.rs").unwrap().len(), 2);
        let second = open(&cache, &path);
        assert_eq!(seeded(&second), Some(true), "no second probe");
        assert_eq!(second.chunks_for_file("src/a.rs").unwrap().len(), 2);

        repopulate_without_sidecar(&path, &table);
        let stale: i64 = Connection::open(&path)
            .unwrap()
            .query_row(
                &format!(
                    "SELECT count(*) FROM \"{}\" WHERE file_path = 'src/a.rs' AND id IN (1, 2)",
                    quote_ident(&path_table_name(&table))
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            stale, 2,
            "the stale sidecar still files ids 1 and 2 under a"
        );

        let after = open(&cache, &path);
        assert_eq!(seeded(&after), None, "a commit retires the verdict");
        let b = after.chunks_for_file("src/b.rs").unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].content, "fn b() {}");
        assert_eq!(after.chunks_for_file("src/a.rs").unwrap().len(), 2);
        assert_eq!(seeded(&open(&cache, &path)), Some(false));
    }

    /// A handle that reads an older snapshot than the version it attached at
    /// must not store its verdict under the newer version.
    #[test]
    fn a_verdict_reached_across_a_commit_is_not_stored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let table = index_two_files(&path);
        let cache = PathSidecarVerdicts::new();

        let reader = open(&cache, &path);
        let snapshot = reader.read_snapshot().unwrap();
        repopulate_without_sidecar(&path, &table);
        assert_eq!(reader.chunks_for_file("src/a.rs").unwrap().len(), 2);
        assert_eq!(
            remembered(&reader),
            Some(true),
            "the old snapshot was paired"
        );
        drop(snapshot);

        let next = open(&cache, &path);
        assert_eq!(seeded(&next), None);
        assert_eq!(next.chunks_for_file("src/b.rs").unwrap().len(), 1);

        let late = open(&cache, &path);
        assert_eq!(seeded(&late), Some(false));
        Connection::open(&path)
            .unwrap()
            .execute_batch(&format!(
                "DELETE FROM \"{}\"",
                quote_ident(&path_table_name(&table))
            ))
            .unwrap();
        assert_eq!(late.chunks_for_file("src/b.rs").unwrap().len(), 1);
        assert_eq!(seeded(&open(&cache, &path)), None);
    }

    /// An observer holds its index file open, so one whose file was deleted
    /// or replaced must not wait for an attach to that same path.
    #[test]
    fn observers_of_deleted_or_replaced_index_files_are_released_on_any_attach() {
        let dir = tempfile::tempdir().unwrap();
        let index = |name: &str| {
            let project = dir.path().join(name);
            std::fs::create_dir(&project).unwrap();
            let path = project.join("index.db");
            index_two_files(&path);
            path
        };
        let (deleted, replaced, other) = (index("deleted"), index("replaced"), index("other"));
        let replacement = index("replacement");
        let cache = PathSidecarVerdicts::new();
        open(&cache, &deleted);
        open(&cache, &replaced);
        let observed = |path: &Path| lock(&cache.files).by_path.contains_key(path);
        let (deleted_key, replaced_key) = (
            std::fs::canonicalize(&deleted).unwrap(),
            std::fs::canonicalize(&replaced).unwrap(),
        );
        assert!(observed(&deleted_key) && observed(&replaced_key));

        std::fs::remove_dir_all(deleted.parent().unwrap()).unwrap();
        std::fs::rename(&replacement, &replaced).unwrap();
        open(&cache, &other);
        assert!(!observed(&deleted_key), "deleted index still held open");
        assert!(!observed(&replaced_key), "replaced index still held open");
        assert!(observed(&std::fs::canonicalize(&other).unwrap()));
    }

    #[test]
    fn prune_releases_a_deleted_index_without_an_attach() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let path = project.join("index.db");
        index_two_files(&path);
        let cache = PathSidecarVerdicts::new();
        open(&cache, &path);
        let key = std::fs::canonicalize(&path).unwrap();
        assert_eq!(cache.prune(), 0, "a live index stays observed");
        assert!(lock(&cache.files).by_path.contains_key(&key));

        std::fs::remove_dir_all(&project).unwrap();
        assert_eq!(cache.prune(), 1);
        assert!(lock(&cache.files).by_path.is_empty());
    }

    #[test]
    fn a_replaced_index_file_is_observed_afresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        index_two_files(&path);
        let replacement = dir.path().join("replacement.db");
        let table = index_two_files(&replacement);
        Connection::open(&replacement)
            .unwrap()
            .execute_batch(&format!(
                "PRAGMA journal_mode = DELETE; DELETE FROM \"{}\"",
                quote_ident(&path_table_name(&table))
            ))
            .unwrap();
        let cache = PathSidecarVerdicts::new();
        let db = open(&cache, &path);
        assert_eq!(db.chunks_for_file("src/a.rs").unwrap().len(), 2);
        assert_eq!(seeded(&open(&cache, &path)), Some(true));

        drop(db);
        for suffix in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        std::fs::rename(&replacement, &path).unwrap();
        let db = open(&cache, &path);
        assert_eq!(seeded(&db), None);
        assert_eq!(db.chunks_for_file("src/a.rs").unwrap().len(), 2);
        assert_eq!(remembered(&db), Some(false));
    }

    /// The daemon's search handle is writable and reads inside a query
    /// snapshot; a verdict reached there is committed state all the same.
    #[test]
    fn a_writable_handle_reading_in_a_snapshot_stores_its_verdict() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        index_two_files(&path);
        let cache = PathSidecarVerdicts::new();
        let db = Database::open_for_model_existing(&path, MODEL, DEFAULT_EMBEDDING_DIM).unwrap();
        cache.attach(&db, &path);
        let snapshot = db.read_snapshot().unwrap();
        assert_eq!(db.chunks_for_file("src/a.rs").unwrap().len(), 2);
        assert_eq!(remembered(&db), Some(true));
        drop(snapshot);
        assert_eq!(seeded(&open(&cache, &path)), Some(true));
    }

    /// A handle's reads can open their snapshot well after its attach (a
    /// query loads its models in between), so a seed must be confirmed for
    /// the snapshot that reads, whichever way that snapshot is opened.
    #[test]
    fn a_seed_is_not_used_by_a_snapshot_opened_after_a_commit() {
        #[derive(Debug, Clone, Copy)]
        enum Read {
            Autocommit,
            Snapshot,
            DeferredBegin,
        }
        for mode in [Read::Autocommit, Read::Snapshot, Read::DeferredBegin] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("index.db");
            let table = index_two_files(&path);
            let cache = PathSidecarVerdicts::new();
            assert_eq!(
                open(&cache, &path)
                    .chunks_for_file("src/a.rs")
                    .unwrap()
                    .len(),
                2
            );
            let db = open(&cache, &path);
            assert_eq!(seeded(&db), Some(true), "{mode:?}");

            swap_sidecar_paths(&path, &table);
            let read_b = |db: &Database| {
                let b = db.chunks_for_file("src/b.rs").unwrap();
                assert_eq!(b.len(), 1, "{mode:?}");
                assert_eq!(b[0].content, "fn b() {}", "{mode:?}");
            };
            match mode {
                Read::Autocommit => read_b(&db),
                Read::Snapshot => {
                    let _snapshot = db.read_snapshot().unwrap();
                    read_b(&db);
                }
                Read::DeferredBegin => db
                    .execute_batch(|db| {
                        read_b(db);
                        Ok(())
                    })
                    .unwrap(),
            }
            assert_eq!(seeded(&db), None, "{mode:?}");
        }
    }

    /// A commit that lands after a deferred `BEGIN` but before its first
    /// read is inside the snapshot that read takes, so the observer must be
    /// consulted after the snapshot is fixed, never before.
    #[test]
    fn a_commit_before_a_deferred_snapshot_is_fixed_retires_the_seed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let table = index_two_files(&path);
        let cache = PathSidecarVerdicts::new();
        assert_eq!(
            open(&cache, &path)
                .chunks_for_file("src/a.rs")
                .unwrap()
                .len(),
            2
        );
        let db = open(&cache, &path);
        assert_eq!(seeded(&db), Some(true));

        let fired = std::rc::Rc::new(std::cell::Cell::new(false));
        let hook = {
            let (path, table, fired) = (path.clone(), table.clone(), fired.clone());
            move || {
                swap_sidecar_paths(&path, &table);
                fired.set(true);
            }
        };
        crate::db::semantic::BEFORE_DEFERRED_SNAPSHOT
            .with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
        db.execute_batch(|db| {
            let b = db.chunks_for_file("src/b.rs")?;
            assert_eq!(b.len(), 1);
            assert_eq!(b[0].content, "fn b() {}");
            Ok(())
        })
        .unwrap();
        assert!(
            fired.get(),
            "the commit landed before the snapshot was fixed"
        );
        assert_eq!(seeded(&db), None);
    }

    /// Without an intervening commit the seed serves every way of reading.
    #[test]
    fn a_confirmed_seed_serves_reads_without_a_probe() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        index_two_files(&path);
        let cache = PathSidecarVerdicts::new();
        assert_eq!(
            open(&cache, &path)
                .chunks_for_file("src/a.rs")
                .unwrap()
                .len(),
            2
        );
        let db = open(&cache, &path);
        assert_eq!(db.chunks_for_file("src/a.rs").unwrap().len(), 2);
        {
            let _snapshot = db.read_snapshot().unwrap();
            assert_eq!(db.chunks_for_file("src/b.rs").unwrap().len(), 1);
        }
        assert_eq!(db.all_chunk_file_paths().unwrap(), ["src/a.rs", "src/b.rs"]);
        assert_eq!(seeded(&db), Some(true));
        assert_eq!(remembered(&db), None, "no probe ran");
        assert!(db.conn.is_autocommit(), "the confirming snapshot closed");
    }

    #[test]
    fn a_handle_inside_a_transaction_is_not_attached() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        index_two_files(&path);
        let cache = PathSidecarVerdicts::new();
        let db = Database::open_read_only_for_existing_model(&path, MODEL).unwrap();
        let snapshot = db.read_snapshot().unwrap();
        cache.attach(&db, &path);
        assert!(db.verified_sidecars.borrow().verdict_sink.is_none());
        assert_eq!(db.chunks_for_file("src/a.rs").unwrap().len(), 2);
        drop(snapshot);
        assert_eq!(seeded(&open(&cache, &path)), None);
    }
}
