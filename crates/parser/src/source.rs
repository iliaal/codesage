use std::fs::{File, Metadata};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

use crate::discover::{MAX_INDEXABLE_FILE_BYTES, content_hash};

pub struct IndexableSource {
    pub bytes: Vec<u8>,
    pub content_hash: String,
}

/// The root is caller-authorized; every descendant is opened without following
/// symlinks. A detected mutation retries the bounded read, never its old hash.
/// Directory-descriptor traversal requires Unix; other platforms refuse the read.
pub fn read_indexable_file(root: &Path, relative: &Path) -> Result<Option<IndexableSource>> {
    read_indexable_file_with(root, relative, || {}, || {})
}

fn read_indexable_file_with(
    root: &Path,
    relative: &Path,
    mut before_open: impl FnMut(),
    mut before_read: impl FnMut(),
) -> Result<Option<IndexableSource>> {
    for attempt in 0..3 {
        let result = (|| {
            let Some(file) = IndexableFile::open_with(root, relative, &mut before_open)? else {
                return Ok(None);
            };
            file.read_with(&mut before_read)
        })();
        if attempt < 2
            && result.as_ref().err().is_some_and(|e: &anyhow::Error| {
                e.downcast_ref::<io::Error>()
                    .is_some_and(|e| e.kind() == io::ErrorKind::Interrupted)
            })
        {
            continue;
        }
        return result;
    }
    unreachable!()
}

pub(crate) struct IndexableFile {
    file: File,
    root: File,
    relative: PathBuf,
    before: Metadata,
}

impl IndexableFile {
    pub(crate) fn open(root: &Path, relative: &Path) -> Result<Option<Self>> {
        Self::open_with(root, relative, || {})
    }

    #[cfg(unix)]
    fn open_with(root: &Path, relative: &Path, before_open: impl FnOnce()) -> Result<Option<Self>> {
        use rustix::fs::{Mode, OFlags, open};

        let root = File::from(
            open(
                root,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(io::Error::from)?,
        );
        let file = open_relative(&root, relative, before_open)?;
        let before = file.metadata()?;
        if before.len() > MAX_INDEXABLE_FILE_BYTES {
            return Ok(None);
        }
        Ok(Some(Self {
            file,
            root,
            relative: relative.to_path_buf(),
            before,
        }))
    }

    #[cfg(not(unix))]
    fn open_with(_: &Path, _: &Path, _: impl FnOnce()) -> Result<Option<Self>> {
        anyhow::bail!("race-resistant indexable file reads require Unix")
    }

    pub(crate) fn metadata(&self) -> &Metadata {
        &self.before
    }

    pub(crate) fn verify(&self) -> Result<()> {
        if !same_snapshot(&self.before, &self.file.metadata()?) {
            return Err(changed_source());
        }
        let current = open_relative(&self.root, &self.relative, || {})?;
        if !same_snapshot(&self.before, &current.metadata()?) {
            return Err(changed_source());
        }
        Ok(())
    }

    pub(crate) fn read(self) -> Result<Option<IndexableSource>> {
        self.read_with(|| {})
    }

    fn read_with(self, before_read: impl FnOnce()) -> Result<Option<IndexableSource>> {
        before_read();
        let mut bytes = Vec::new();
        (&self.file)
            .take(MAX_INDEXABLE_FILE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_INDEXABLE_FILE_BYTES {
            return Ok(None);
        }
        self.verify()?;
        ensure!(
            bytes.len() as u64 == self.before.len(),
            "indexable file length differs from its source snapshot"
        );
        Ok(Some(IndexableSource {
            content_hash: content_hash(&bytes),
            bytes,
        }))
    }
}

fn changed_source() -> anyhow::Error {
    io::Error::new(
        io::ErrorKind::Interrupted,
        "indexable file changed while taking its source snapshot",
    )
    .into()
}

#[cfg(unix)]
fn same_snapshot(a: &Metadata, b: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    a.is_file()
        && b.is_file()
        && a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}

#[cfg(not(unix))]
fn same_snapshot(_: &Metadata, _: &Metadata) -> bool {
    false
}

#[cfg(unix)]
fn open_relative(root: &File, relative: &Path, before_open: impl FnOnce()) -> Result<File> {
    use rustix::fs::{AtFlags, FileType, Mode, OFlags, fstat, openat, statat};

    ensure!(
        !relative.as_os_str().is_empty()
            && relative
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_))),
        "indexable path must be relative and contain no parent traversal"
    );
    let mut dir = root.try_clone()?;
    let parent = relative.parent().context("indexable path has no parent")?;
    for component in parent {
        dir = File::from(
            openat(
                &dir,
                component,
                OFlags::RDONLY
                    | OFlags::DIRECTORY
                    | OFlags::NOFOLLOW
                    | OFlags::NONBLOCK
                    | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(io::Error::from)?,
        );
    }
    let leaf = relative
        .file_name()
        .context("indexable path has no file name")?;
    let expected = statat(&dir, leaf, AtFlags::SYMLINK_NOFOLLOW).map_err(io::Error::from)?;
    ensure!(
        FileType::from_raw_mode(expected.st_mode) == FileType::RegularFile,
        "indexable path is not a regular file"
    );
    before_open();
    let file = File::from(
        openat(
            &dir,
            leaf,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)?,
    );
    let actual = fstat(&file).map_err(io::Error::from)?;
    ensure!(
        FileType::from_raw_mode(actual.st_mode) == FileType::RegularFile,
        "indexable descriptor is not a regular file"
    );
    if expected.st_dev != actual.st_dev || expected.st_ino != actual.st_ino {
        return Err(changed_source());
    }
    Ok(file)
}

#[cfg(not(unix))]
fn open_relative(_: &File, _: &Path, _: impl FnOnce()) -> Result<File> {
    anyhow::bail!("race-resistant indexable file reads require Unix")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Seek;
    use std::os::unix::fs::symlink;
    use std::sync::mpsc;
    use std::time::Duration;

    fn replace(path: &Path, source: &[u8]) {
        let replacement = path.with_extension("replacement");
        std::fs::write(&replacement, source).unwrap();
        std::fs::rename(replacement, path).unwrap();
    }

    #[test]
    fn indexable_reader_preserves_not_found_error_kind() {
        let root = tempfile::tempdir().unwrap();
        for relative in ["missing.rs", "missing/child.rs"] {
            let error = read_indexable_file(root.path(), Path::new(relative))
                .err()
                .expect("missing source must fail");
            assert_eq!(
                error.downcast_ref::<io::Error>().map(io::Error::kind),
                Some(io::ErrorKind::NotFound),
                "{error:#}"
            );
        }
        let error = read_indexable_file(&root.path().join("missing"), Path::new("child.rs"))
            .err()
            .expect("missing root must fail");
        assert_eq!(
            error.downcast_ref::<io::Error>().map(io::Error::kind),
            Some(io::ErrorKind::NotFound),
            "{error:#}"
        );
    }

    #[test]
    fn indexable_reader_preserves_exact_bytes_and_hash() {
        let root = tempfile::tempdir().unwrap();
        let bytes = b"// caf\xe9\nfn live() {}\n";
        std::fs::write(root.path().join("live.rs"), bytes).unwrap();
        let source = read_indexable_file(root.path(), Path::new("live.rs"))
            .unwrap()
            .unwrap();
        assert_eq!(source.bytes, bytes);
        assert_eq!(source.content_hash, content_hash(bytes));
    }

    #[test]
    fn indexable_reader_preserves_an_authorized_root_alias() {
        let root = tempfile::tempdir().unwrap();
        let aliases = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("live.rs"), b"fn live() {}\n").unwrap();
        let alias = aliases.path().join("project");
        symlink(root.path(), &alias).unwrap();
        let source = read_indexable_file(&alias, Path::new("live.rs"))
            .unwrap()
            .unwrap();
        assert_eq!(source.bytes, b"fn live() {}\n");
    }

    #[test]
    fn indexable_reader_accepts_the_size_boundary() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("boundary.rs");
        std::fs::File::create(path)
            .unwrap()
            .set_len(MAX_INDEXABLE_FILE_BYTES)
            .unwrap();
        let source = read_indexable_file(root.path(), Path::new("boundary.rs"))
            .unwrap()
            .unwrap();
        assert_eq!(source.bytes.len() as u64, MAX_INDEXABLE_FILE_BYTES);
        assert_eq!(source.content_hash, content_hash(&source.bytes));
    }

    #[test]
    fn indexable_reader_caps_bytes_even_when_growth_follows_metadata_check() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("growth.rs");
        std::fs::write(&path, b"fn small() {}\n").unwrap();
        let file = IndexableFile::open(root.path(), Path::new("growth.rs"))
            .unwrap()
            .unwrap();
        let mut position = file.file.try_clone().unwrap();
        let source = file
            .read_with(|| {
                std::fs::File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_len(MAX_INDEXABLE_FILE_BYTES * 4)
                    .unwrap();
            })
            .unwrap();
        assert!(source.is_none());
        assert_eq!(
            position.stream_position().unwrap(),
            MAX_INDEXABLE_FILE_BYTES + 1
        );
    }

    #[test]
    fn indexable_reader_retries_replacement_between_check_and_open() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("replace.rs");
        std::fs::write(&path, b"fn before() {}\n").unwrap();
        let mut pending = true;
        let source = read_indexable_file_with(
            root.path(),
            Path::new("replace.rs"),
            || {
                if pending {
                    pending = false;
                    replace(&path, b"fn after() {}\n");
                }
            },
            || {},
        )
        .unwrap()
        .unwrap();
        assert!(!pending);
        assert_eq!(source.bytes, b"fn after() {}\n");
        assert_eq!(source.content_hash, content_hash(b"fn after() {}\n"));
    }

    #[test]
    fn indexable_reader_retries_a_replaced_open_descriptor() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("replace.rs");
        std::fs::write(&path, b"fn before() {}\n").unwrap();
        let mut pending = true;
        let source = read_indexable_file_with(
            root.path(),
            Path::new("replace.rs"),
            || {},
            || {
                if pending {
                    pending = false;
                    replace(&path, b"fn after() {}\n");
                }
            },
        )
        .unwrap()
        .unwrap();
        assert!(!pending);
        assert_eq!(source.bytes, b"fn after() {}\n");
        assert_eq!(source.content_hash, content_hash(b"fn after() {}\n"));
    }

    #[test]
    fn indexable_reader_bounds_retries_for_a_continuously_replaced_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("changing.rs");
        std::fs::write(&path, b"fn initial() {}\n").unwrap();
        let mut replacements = 0;
        let result = read_indexable_file_with(
            root.path(),
            Path::new("changing.rs"),
            || {},
            || {
                replacements += 1;
                replace(
                    &path,
                    format!("fn replaced_{replacements}() {{}}\n").as_bytes(),
                );
            },
        );
        assert!(result.is_err());
        assert_eq!(replacements, 3);
    }

    #[test]
    fn indexable_reader_retries_in_place_rewrites_with_restored_mtime() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("rewrite.rs");
        std::fs::write(&path, b"fn before() {}\n").unwrap();
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        let mut pending = true;
        let source = read_indexable_file_with(
            root.path(),
            Path::new("rewrite.rs"),
            || {},
            || {
                if pending {
                    pending = false;
                    std::fs::write(&path, b"fn after_() {}\n").unwrap();
                    std::fs::File::options()
                        .write(true)
                        .open(&path)
                        .unwrap()
                        .set_times(std::fs::FileTimes::new().set_modified(mtime))
                        .unwrap();
                }
            },
        )
        .unwrap()
        .unwrap();
        assert!(!pending);
        assert_eq!(source.bytes, b"fn after_() {}\n");
        assert_eq!(source.content_hash, content_hash(b"fn after_() {}\n"));
    }

    #[test]
    fn indexable_reader_never_blocks_on_a_fifo_substituted_before_open() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("fifo.rs");
        std::fs::write(&path, b"fn before() {}\n").unwrap();
        let root_path = root.path().to_path_buf();
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = IndexableFile::open_with(&root_path, Path::new("fifo.rs"), || {
                std::fs::remove_file(&path).unwrap();
                rustix::fs::mkfifoat(
                    rustix::fs::CWD,
                    &path,
                    rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
                )
                .unwrap();
            });
            tx.send(result.is_err()).unwrap();
        });
        assert!(
            rx.recv_timeout(Duration::from_secs(2))
                .expect("FIFO open must return without a writer")
        );
        worker.join().unwrap();
    }

    #[test]
    fn indexable_reader_refuses_leaf_symlinks_before_and_during_open() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let path = root.path().join("link.rs");
        let target = outside.path().join("target.rs");
        std::fs::write(&target, b"fn forbidden_target() {}\n").unwrap();
        symlink(&target, &path).unwrap();
        assert!(read_indexable_file(root.path(), Path::new("link.rs")).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"fn before() {}\n").unwrap();
        let result = IndexableFile::open_with(root.path(), Path::new("link.rs"), || {
            std::fs::remove_file(&path).unwrap();
            symlink(&target, &path).unwrap();
        });
        assert!(result.is_err());
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"fn forbidden_target() {}\n"
        );
    }

    #[test]
    fn indexable_reader_refuses_static_and_substituted_ancestor_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(
            outside.path().join("file.rs"),
            b"fn forbidden_target() {}\n",
        )
        .unwrap();
        let ancestor = root.path().join("nested");
        symlink(outside.path(), &ancestor).unwrap();
        assert!(read_indexable_file(root.path(), Path::new("nested/file.rs")).is_err());
        std::fs::remove_file(&ancestor).unwrap();
        std::fs::create_dir(&ancestor).unwrap();
        std::fs::write(ancestor.join("file.rs"), b"fn before() {}\n").unwrap();
        let file = IndexableFile::open_with(root.path(), Path::new("nested/file.rs"), || {
            std::fs::rename(&ancestor, root.path().join("retained")).unwrap();
            symlink(outside.path(), &ancestor).unwrap();
        })
        .unwrap()
        .unwrap();
        assert!(file.read().is_err());
    }

    #[test]
    fn indexable_reader_refuses_nonregular_files_and_parent_traversal() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("directory.rs")).unwrap();
        assert!(read_indexable_file(root.path(), Path::new("directory.rs")).is_err());
        assert!(read_indexable_file(root.path(), Path::new("../escape.rs")).is_err());
        assert!(read_indexable_file(root.path(), &root.path().join("absolute.rs")).is_err());
    }
}
