//! Coordinate index writers with a non-blocking advisory lock at `.codesage/indexing.lock`.
//! Contending commands exit 75 (EX_TEMPFAIL); hook callers may wait to avoid missing history updates.

use std::fs::{File, OpenOptions};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Outcome of a non-blocking lock acquisition attempt.
pub enum LockOutcome {
    /// We got the lock. Keep the [`IndexLock`] alive for the write
    /// duration — the OS holds the `flock` while the `File` is open.
    Acquired(IndexLock),
    /// Another process already holds the lock on this project.
    AlreadyHeld,
}

/// Hold through the write; file close releases the advisory lock.
#[must_use = "dropping an IndexLock releases the project write lock; keep \
              it alive until the write completes"]
pub struct IndexLock {
    _file: File,
}

/// Try the per-project advisory lock without waiting. File contents are irrelevant.
/// `AlreadyHeld` distinguishes contention from I/O errors.
pub fn try_acquire(project_root: &Path) -> Result<LockOutcome> {
    let codesage_dir = project_root.join(".codesage");
    if !codesage_dir.is_dir() {
        // Let the command report a missing index instead of an unrelated lockfile error.
        return Ok(LockOutcome::Acquired(IndexLock {
            _file: empty_file_handle()?,
        }));
    }
    let path = codesage_dir.join("indexing.lock");
    // Repository-controlled lock paths may be planted symlinks.
    let file = crate::fsguard::open_lockfile(&path)
        .with_context(|| format!("opening lockfile {}", path.display()))?;
    // Android stdlib try_lock lacks the required cfg through Rust 1.95; use flock directly.
    #[cfg(target_os = "android")]
    {
        match crate::flock_override::try_flock_exclusive(&file) {
            Ok(()) => Ok(LockOutcome::Acquired(IndexLock { _file: file })),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(LockOutcome::AlreadyHeld),
            Err(e) => Err(anyhow::Error::from(e).context(format!("flock on {}", path.display()))),
        }
    }
    #[cfg(not(target_os = "android"))]
    {
        match file.try_lock() {
            Ok(()) => Ok(LockOutcome::Acquired(IndexLock { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(LockOutcome::AlreadyHeld),
            Err(std::fs::TryLockError::Error(e)) => {
                Err(anyhow::Error::from(e).context(format!("try_lock on {}", path.display())))
            }
        }
    }
}

/// Wait for watcher contention so commit hooks can refresh history and feature maps.
/// Zero wait is a single non-blocking attempt.
pub fn acquire_with_wait(project_root: &Path, wait: Duration) -> Result<LockOutcome> {
    let deadline = Instant::now() + wait;
    loop {
        match try_acquire(project_root)? {
            LockOutcome::Acquired(lock) => return Ok(LockOutcome::Acquired(lock)),
            LockOutcome::AlreadyHeld => {
                let now = Instant::now();
                if now >= deadline {
                    return Ok(LockOutcome::AlreadyHeld);
                }
                std::thread::sleep(POLL_INTERVAL.min(deadline - now));
            }
        }
    }
}

/// Inert handle for non-onboarded projects; no lock is acquired.
fn empty_file_handle() -> Result<File> {
    OpenOptions::new()
        .read(true)
        .open(if cfg!(unix) { "/dev/null" } else { "NUL" })
        .context("opening null device for no-op IndexLock")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[cfg(unix)]
    #[test]
    fn acquire_refuses_a_symlinked_lockfile() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        let target = root.join("created-by-attacker");
        std::os::unix::fs::symlink(&target, root.join(".codesage/indexing.lock")).unwrap();

        assert!(try_acquire(root).is_err());
        assert!(!target.exists(), "the lock open created the link's target");
    }

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
        // Concurrent test forks may retain the lock fd until exec, briefly outliving our drop.
        drop(first);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match try_acquire(root).unwrap() {
                LockOutcome::Acquired(_) => break, // expected — lock round-trips
                LockOutcome::AlreadyHeld if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                LockOutcome::AlreadyHeld => panic!("lock was not released on drop within 2s"),
            }
        }
    }

    #[test]
    fn try_acquire_on_non_onboarded_project_succeeds_noop() {
        let tmp = tempfile::tempdir().unwrap();
        match try_acquire(tmp.path()).unwrap() {
            LockOutcome::Acquired(_) => { /* expected */ }
            LockOutcome::AlreadyHeld => panic!("no-op branch should never report contention"),
        }
    }

    #[test]
    fn acquire_with_wait_zero_matches_try_acquire() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".codesage")).unwrap();
        let _first = match try_acquire(tmp.path()).unwrap() {
            LockOutcome::Acquired(l) => l,
            LockOutcome::AlreadyHeld => unreachable!(),
        };
        let t0 = std::time::Instant::now();
        match acquire_with_wait(tmp.path(), Duration::ZERO).unwrap() {
            LockOutcome::AlreadyHeld => { /* expected */ }
            LockOutcome::Acquired(_) => panic!("lock is held; zero-wait must not acquire"),
        }
        assert!(
            t0.elapsed() < Duration::from_millis(200),
            "zero-wait acquire took {:?} — it must not poll",
            t0.elapsed()
        );
    }

    #[test]
    fn acquire_with_wait_times_out_when_lock_never_released() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".codesage")).unwrap();
        let _first = match try_acquire(tmp.path()).unwrap() {
            LockOutcome::Acquired(l) => l,
            LockOutcome::AlreadyHeld => unreachable!(),
        };
        let wait = Duration::from_millis(300);
        let t0 = std::time::Instant::now();
        match acquire_with_wait(tmp.path(), wait).unwrap() {
            LockOutcome::AlreadyHeld => { /* expected — bounded skip */ }
            LockOutcome::Acquired(_) => panic!("lock is held for the whole window"),
        }
        assert!(
            t0.elapsed() >= wait,
            "returned after {:?}, before the {wait:?} window elapsed",
            t0.elapsed()
        );
    }

    #[test]
    fn acquire_with_wait_acquires_after_holder_releases() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".codesage")).unwrap();
        let root = tmp.path().to_path_buf();

        let first = match try_acquire(&root).unwrap() {
            LockOutcome::Acquired(l) => l,
            LockOutcome::AlreadyHeld => unreachable!(),
        };
        let holder = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(700));
            drop(first);
        });

        match acquire_with_wait(tmp.path(), Duration::from_secs(10)).unwrap() {
            LockOutcome::Acquired(_) => { /* expected — waited out the holder */ }
            LockOutcome::AlreadyHeld => {
                panic!("holder released within the window; wait should have acquired")
            }
        }
        holder.join().unwrap();
    }

    #[test]
    fn sleep_to_ensure_lock_ordering_is_deterministic() {
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
