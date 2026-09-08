//! State replacement and log appends shared by graph and CLI writers.

use std::fs::{File, OpenOptions, Permissions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

fn target_permissions(path: &Path) -> io::Result<Option<Permissions>> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => Ok(Some(meta.permissions())),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn parent(path: &Path) -> io::Result<&Path> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("state file has no parent"))?;
    if !std::fs::symlink_metadata(dir)?.is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a directory",
            dir.display()
        )));
    }
    Ok(dir)
}

/// Replace a regular state file through an exclusive, same-directory temporary.
/// Existing permissions survive replacement; new files are private (0600).
/// Callers retain responsibility for ancestor-directory trust and writer locks.
pub fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    replace_with(path, |file| file.write_all(bytes))
}

fn replace_with(path: &Path, write: impl FnOnce(&mut File) -> io::Result<()>) -> io::Result<()> {
    let dir = parent(path)?;
    let permissions = target_permissions(path)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".codesage-state-")
        .tempfile_in(dir)?;
    write(tmp.as_file_mut())?;
    if let Some(permissions) = permissions {
        tmp.as_file().set_permissions(permissions)?;
    }
    tmp.as_file().sync_all()?;
    target_permissions(path)?;
    tmp.persist(path).map_err(|e| e.error)?;
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    Ok(())
}

/// Open a regular file without following its leaf or immediate parent symlink.
pub fn open(path: &Path, append: bool) -> io::Result<File> {
    parent(path)?;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .append(append);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(not(unix))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "symlink-safe state opens require Unix",
    ));
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("state target is not a regular file"));
    }
    Ok(file)
}

/// Hold a separate inode across rotation and append, so rename cannot split locks.
pub fn lock(path: &Path) -> io::Result<File> {
    let file = open(path, false)?;
    #[cfg(target_os = "android")]
    flock_exclusive(&file)?;
    #[cfg(not(target_os = "android"))]
    file.lock()?;
    Ok(file)
}

// Rust <=1.95 omits Android from File::lock's supported platforms.
#[cfg(any(target_os = "android", all(test, unix)))]
fn flock_exclusive(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: the borrowed File keeps this descriptor valid for the flock call.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Append one complete JSON line while the caller holds its log lock.
/// A crashed append's unterminated tail must not consume the next valid record.
pub fn append_line(path: &Path, line: &[u8]) -> io::Result<()> {
    let mut file = open(path, true)?;
    if file.metadata()?.len() > 0 {
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0];
        file.read_exact(&mut last)?;
        if last[0] != b'\n' {
            file.write_all(b"\n")?;
        }
    }
    file.write_all(line)?;
    file.sync_data()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(unix, not(target_os = "android")))]
    #[test]
    fn android_flock_fallback_excludes_another_writer_until_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        let first = open(&path, false).unwrap();
        let second = open(&path, false).unwrap();
        flock_exclusive(&first).unwrap();
        assert!(matches!(
            second.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        drop(first);
        second.lock().unwrap();
    }

    #[test]
    fn interrupted_replacement_keeps_old_state_and_removes_temporary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        std::fs::write(&path, b"old complete state").unwrap();
        let error = replace_with(&path, |file| {
            file.write_all(b"partial")?;
            Err(io::Error::other("injected write failure"))
        });
        assert!(error.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"old complete state");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn replacement_exposes_old_state_until_the_complete_write_is_published() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        std::fs::write(&path, b"old").unwrap();
        replace_with(&path, |file| {
            file.write_all(b"new")?;
            assert_eq!(std::fs::read(&path).unwrap(), b"old");
            file.write_all(b" complete")
        })
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new complete");
    }

    #[cfg(unix)]
    #[test]
    fn replacement_preserves_mode_and_refuses_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        replace(&path, b"old").unwrap();
        std::fs::set_permissions(&path, Permissions::from_mode(0o640)).unwrap();
        replace(&path, b"new").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        let link = dir.path().join("link");
        symlink(&path, &link).unwrap();
        assert!(replace(&link, b"bad").is_err());
        assert!(append_line(&link, b"bad\n").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn concurrent_replacements_publish_only_whole_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        replace(&path, &vec![b'a'; 8192]).unwrap();
        std::thread::scope(|scope| {
            for byte in b'a'..=b'd' {
                let path = &path;
                scope.spawn(move || {
                    for _ in 0..20 {
                        replace(path, &vec![byte; 8192]).unwrap();
                        let bytes = std::fs::read(path).unwrap();
                        assert_eq!(bytes.len(), 8192);
                        assert!(bytes.iter().all(|b| *b == bytes[0]));
                    }
                });
            }
        });
    }

    #[test]
    fn append_preserves_records_on_both_sides_of_a_malformed_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        std::fs::write(&path, b"{\"n\":1}\n{\"n\":").unwrap();
        append_line(&path, b"{\"n\":2}\n").unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let rows: Vec<serde_json::Value> = raw
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        assert_eq!(
            rows,
            vec![serde_json::json!({"n":1}), serde_json::json!({"n":2})]
        );
    }
}
