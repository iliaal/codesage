//! Repository state paths are untrusted: clones can plant symlinks under `.codesage/`.
//! O_NOFOLLOW rejects leaf symlinks at open time; parent checks are separate.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

/// Options with `O_NOFOLLOW` set on Unix, unchanged elsewhere.
pub(crate) fn no_follow_options() -> OpenOptions {
    let mut opts = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // O_NONBLOCK prevents FIFOs from blocking before the file-type check.
        opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    opts
}

/// Non-Unix MCP still runs, but these state guards lack a safe implementation there.
#[cfg(not(unix))]
fn unsupported_platform(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "refusing to touch project state at {}: symlink-safe file access is implemented for Unix only",
            path.display()
        ),
    )
}

/// Open a regular, non-symlink state file for locking; refuse unsupported platforms.
pub(crate) fn open_lockfile(path: &Path) -> io::Result<File> {
    #[cfg(not(unix))]
    {
        return Err(unsupported_platform(path));
    }
    #[cfg(unix)]
    {
        reject_symlinked_project_dir(path)?;
        let file = no_follow_options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        require_regular_file(&file, path)?;
        Ok(file)
    }
}

fn require_regular_file(file: &File, path: &Path) -> io::Result<()> {
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    Ok(())
}

/// O_NOFOLLOW protects only the leaf; reject a symlinked `.codesage` parent separately.
pub(crate) fn reject_symlinked_project_dir(path: &Path) -> io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.file_name().and_then(|n| n.to_str()) != Some(crate::PROJECT_DIR) {
        return Ok(());
    }
    let is_symlink = std::fs::symlink_metadata(parent)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    if is_symlink {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is a symlink", parent.display()),
        ));
    }
    Ok(())
}

/// Create or truncate `path` for writing, refusing to follow a symlink at the
/// final component or at a symlinked `.codesage` parent.
pub(crate) fn create_no_follow(path: &Path) -> io::Result<File> {
    #[cfg(not(unix))]
    {
        return Err(unsupported_platform(path));
    }
    #[cfg(unix)]
    {
        reject_symlinked_project_dir(path)?;
        let file = no_follow_options()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        require_regular_file(&file, path)?;
        Ok(file)
    }
}

/// Bound memory consumption from untrusted marker and config files.
pub(crate) const MAX_STATE_BYTES: u64 = 1 << 20;

/// Read bounded regular state files only; symlinked devices can stream forever
/// and FIFOs can block before a read.
pub(crate) fn read_state_to_string(path: &Path) -> io::Result<String> {
    use std::io::Read as _;

    #[cfg(not(unix))]
    {
        return Err(unsupported_platform(path));
    }
    reject_symlinked_project_dir(path)?;
    let file = no_follow_options().read(true).open(path)?;
    require_regular_file(&file, path)?;
    let meta = file.metadata()?;
    if meta.len() > MAX_STATE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} is too large ({} bytes, max {MAX_STATE_BYTES})",
                path.display(),
                meta.len()
            ),
        ));
    }
    // A file may grow after stat; bound the read itself.
    let mut buf = String::new();
    file.take(MAX_STATE_BYTES + 1).read_to_string(&mut buf)?;
    if buf.len() as u64 > MAX_STATE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} exceeded {MAX_STATE_BYTES} bytes while reading",
                path.display()
            ),
        ));
    }
    Ok(buf)
}

/// Unlinking a leaf symlink leaves its target intact; guard only the project parent.
pub(crate) fn remove_state_file(path: &Path) -> io::Result<()> {
    reject_symlinked_project_dir(path)?;
    std::fs::remove_file(path)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn create_no_follow_refuses_a_symlinked_target() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        std::fs::write(&victim, b"keep me").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&victim, &link).unwrap();

        assert!(create_no_follow(&link).is_err());
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep me");
    }

    #[test]
    fn create_no_follow_refuses_a_dangling_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("absent");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(create_no_follow(&link).is_err());
        assert!(!target.exists());
    }

    #[test]
    fn create_no_follow_refuses_a_symlinked_project_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir(&root).unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join(crate::PROJECT_DIR)).unwrap();

        let target = root.join(crate::PROJECT_DIR).join("watch.status");
        assert!(create_no_follow(&target).is_err());
        assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
    }

    #[test]
    fn read_state_refuses_a_symlinked_source() {
        // Use a regular target so removing O_NOFOLLOW fails deterministically instead of hanging on /dev/zero.
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim.toml");
        std::fs::write(&victim, b"secret = true").unwrap();
        let link = dir.path().join("config.toml");
        std::os::unix::fs::symlink(&victim, &link).unwrap();

        assert!(read_state_to_string(&link).is_err());
    }

    #[test]
    fn read_state_refuses_an_oversized_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, vec![b'x'; (MAX_STATE_BYTES + 1) as usize]).unwrap();

        assert!(read_state_to_string(&path).is_err());
    }

    #[test]
    fn read_state_reads_an_ordinary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, b"hello").unwrap();

        assert_eq!(read_state_to_string(&path).unwrap(), "hello");
    }

    #[test]
    fn remove_state_file_refuses_a_symlinked_project_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir(&root).unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let victim = outside.join("watch.disabled");
        std::fs::write(&victim, b"x").unwrap();
        std::os::unix::fs::symlink(&outside, root.join(crate::PROJECT_DIR)).unwrap();

        let target = root.join(crate::PROJECT_DIR).join("watch.disabled");
        assert!(remove_state_file(&target).is_err());
        assert!(victim.exists(), "the unlink reached through the symlink");
    }

    #[test]
    fn create_no_follow_writes_an_ordinary_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain");
        {
            let mut f = create_no_follow(&path).unwrap();
            std::io::Write::write_all(&mut f, b"hello").unwrap();
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
    }
}
