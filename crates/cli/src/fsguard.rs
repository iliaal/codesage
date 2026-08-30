//! Opening files that live inside a repository work tree.
//!
//! CodeSage keeps its per-project state in `<repo>/.codesage/`, which means a
//! cloned third-party repository can ship any of those names as a symlink (git
//! stores symlinks as mode-120000 blobs and checkout materializes them). Every
//! writer under that directory must therefore refuse to follow a link out of
//! the tree. `O_NOFOLLOW` is the race-free way to do it: the kernel fails the
//! open with `ELOOP` instead of leaving a window between an lstat check and the
//! write.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

/// Options with `O_NOFOLLOW` set on Unix, unchanged elsewhere.
pub(crate) fn no_follow_options() -> OpenOptions {
    let mut opts = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // O_NONBLOCK alongside O_NOFOLLOW: a regular file ignores it, but it
        // stops a non-regular target from blocking the open before the
        // file-type check below can reject it.
        opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    opts
}

/// Fail closed on targets without `O_NOFOLLOW`.
///
/// `codesage mcp` has a deliberate non-Unix path (it runs the MCP server
/// directly, without the daemon), but none of the guards in this module have a
/// non-Unix implementation: `OpenOptions` there would follow links and reparse
/// points silently. Refuse rather than degrade without saying so.
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

/// Open a `.codesage/` state file for locking: no symlink, regular file only.
///
/// `no_follow_options()` is a plain `OpenOptions` off-Unix, so callers that use
/// it directly must go through here rather than reimplementing the checks.
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

/// Reject an opened handle that is not a regular file.
fn require_regular_file(file: &File, path: &Path) -> io::Result<()> {
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    Ok(())
}

/// Refuse to write into a `.codesage` directory that is itself a symlink.
///
/// `O_NOFOLLOW` only guards the final path component, so without this a planted
/// `.codesage` directory symlink still redirects every fixed-name file below it
/// (`watch.status`, `indexing.lock`, `feature-map.state`, …) out of the tree.
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

/// Largest project-state file any of the readers below will accept.
///
/// The files are our own small markers and configs; the cap exists so a
/// repo-planted character device or a huge regular file cannot exhaust memory.
pub(crate) const MAX_STATE_BYTES: u64 = 1 << 20;

/// Read a `.codesage/` state file without following symlinks and without
/// trusting its size.
///
/// `fs::read_to_string` on a repo-controlled path is its own bypass class: a
/// planted `config.toml -> /dev/zero` yields valid UTF-8 NUL bytes forever
/// (unbounded memory), and a fifo blocks the caller indefinitely. Neither is a
/// write primitive, so the O_NOFOLLOW write guards do not cover it. Refuse a
/// symlinked `.codesage` parent, open the leaf with O_NOFOLLOW, require a
/// regular file, and cap the read.
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
    // Cap the read itself too: the size check above is advisory for anything
    // whose length does not describe how much it will hand us.
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

/// Remove a `.codesage/` state file, refusing a symlinked project directory.
///
/// Unlinking a symlinked *leaf* removes the link, not its target, so only the
/// parent needs guarding here.
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
        // The case that matters is a character device such as /dev/zero, whose
        // NUL bytes are valid UTF-8 forever. This points at an ordinary file on
        // purpose: reverting O_NOFOLLOW must fail deterministically here rather
        // than hang the test suite.
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
