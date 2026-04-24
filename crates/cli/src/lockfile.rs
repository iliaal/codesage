//! Advisory lockfile coordination for CodeSage indexing commands.
//!
//! Two `codesage index` / `git-index` / `cleanup` processes running against
//! the same `.codesage/index.db` used to collide at the SQLite layer —
//! WAL mode + `ON DELETE CASCADE` prevents corruption (verified by
//! `bench/concurrency-audit.py`, recommendations doc §2.4), but the
//! losing process exits with a scary-looking `Error: database is locked`.
//! That matters most for `install-hooks`-registered background hooks
//! that fire on every commit / merge / checkout and often overlap a
//! manual indexing run.
//!
//! Fix: a per-project advisory lockfile at `.codesage/indexing.lock`,
//! acquired non-blocking on entry to any writer-style command. If
//! another process already holds it, the second instance exits 0
//! quietly rather than waiting or erroring. The lock is released when
//! the `IndexLock` value is dropped (file-handle close).
//!
//! Uses the stdlib `File::try_lock` API (stable since Rust 1.89) so no
//! new crate is pulled in.

use std::fs::{File, OpenOptions};
use std::path::Path;

use anyhow::{Context, Result};

/// Outcome of a non-blocking lock acquisition attempt.
pub enum LockOutcome {
    /// We got the lock. Keep the [`IndexLock`] alive for the write
    /// duration — the OS holds the `flock` while the `File` is open.
    Acquired(IndexLock),
    /// Another process already holds the lock on this project.
    AlreadyHeld,
}

/// Handle to an acquired indexing lock. Releases on drop (file close).
/// Holding an `IndexLock` gives the caller exclusive rights to mutate
/// `.codesage/index.db` for the lifetime of the handle; other writers
/// will see `AlreadyHeld`.
#[must_use = "dropping an IndexLock releases the project write lock; keep \
              it alive until the write completes"]
pub struct IndexLock {
    _file: File,
}

/// Attempt to acquire the indexing lock on `<project_root>/.codesage/indexing.lock`
/// without blocking. Creates the file if it doesn't exist; the file's
/// contents are irrelevant — the lock is the OS-level advisory flock on
/// the open handle.
///
/// Returns:
/// - `Ok(LockOutcome::Acquired(lock))` — exclusive lock held; caller
///   owns the write window and must keep `lock` alive while writing.
/// - `Ok(LockOutcome::AlreadyHeld)` — another process holds it; caller
///   should exit 0 with a skip message.
/// - `Err(...)` — unexpected IO error (permissions, disk full, etc.);
///   not the "contention" case.
pub fn try_acquire(project_root: &Path) -> Result<LockOutcome> {
    let codesage_dir = project_root.join(".codesage");
    if !codesage_dir.is_dir() {
        // No `.codesage/` — caller is running in a non-onboarded
        // project; let the command below produce its own clearer
        // error rather than manufacturing a generic one here.
        return Ok(LockOutcome::Acquired(IndexLock {
            _file: empty_file_handle()?,
        }));
    }
    let path = codesage_dir.join("indexing.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening lockfile {}", path.display()))?;
    // `File::try_lock` returns `Ok(())` on success and
    // `Err(TryLockError::WouldBlock)` on contention. Any other error
    // is a real IO failure we want to surface.
    match file.try_lock() {
        Ok(()) => Ok(LockOutcome::Acquired(IndexLock { _file: file })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(LockOutcome::AlreadyHeld),
        Err(std::fs::TryLockError::Error(e)) => Err(anyhow::Error::from(e)
            .context(format!("try_lock on {}", path.display()))),
    }
}

/// Returns an open file handle on `/dev/null`-equivalent that holds a
/// permissive lock for the non-onboarded-dir branch. Kept tiny so the
/// `Acquired` variant remains a value we can construct even when
/// lockfile bookkeeping is skipped.
fn empty_file_handle() -> Result<File> {
    // std tempfile::NamedTempFile would work but we don't want the
    // extra crate; a bare anonymous pipe / dev-null file handle is
    // enough because we don't call try_lock on it again.
    OpenOptions::new()
        .read(true)
        .open(if cfg!(unix) { "/dev/null" } else { "NUL" })
        .context("opening null device for no-op IndexLock")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn second_acquire_gets_already_held() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();

        let first = match try_acquire(root).unwrap() {
            LockOutcome::Acquired(l) => l,
            LockOutcome::AlreadyHeld => panic!("first acquire must succeed on fresh tmpdir"),
        };
        match try_acquire(root).unwrap() {
            LockOutcome::AlreadyHeld => { /* expected */ }
            LockOutcome::Acquired(_) => panic!(
                "second acquire must see AlreadyHeld while first is alive; \
                 stdlib flock semantics were supposed to prevent this"
            ),
        }
        // Releasing the first lock lets the next attempt succeed.
        drop(first);
        match try_acquire(root).unwrap() {
            LockOutcome::Acquired(_) => { /* expected — lock round-trips */ }
            LockOutcome::AlreadyHeld => panic!("lock was not released on drop"),
        }
    }

    #[test]
    fn try_acquire_on_non_onboarded_project_succeeds_noop() {
        // No `.codesage/` in the tmpdir. The command path would then
        // fail with a clearer error about the missing index DB; this
        // test just confirms we don't manufacture a spurious lock
        // error when the project isn't onboarded.
        let tmp = tempfile::tempdir().unwrap();
        match try_acquire(tmp.path()).unwrap() {
            LockOutcome::Acquired(_) => { /* expected */ }
            LockOutcome::AlreadyHeld => panic!("no-op branch should never report contention"),
        }
    }

    #[test]
    fn sleep_to_ensure_lock_ordering_is_deterministic() {
        // Regression guard: try_lock must be non-blocking. If the stdlib
        // API ever shifts semantics, this sleep would expose the drift by
        // making the second call wait.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".codesage")).unwrap();
        let first = match try_acquire(tmp.path()).unwrap() {
            LockOutcome::Acquired(l) => l,
            LockOutcome::AlreadyHeld => unreachable!(),
        };
        let t0 = std::time::Instant::now();
        let _ = try_acquire(tmp.path()).unwrap();
        assert!(
            t0.elapsed() < Duration::from_millis(200),
            "try_acquire took {:?} — non-blocking semantics likely broken",
            t0.elapsed()
        );
        drop(first);
    }
}
