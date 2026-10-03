use std::path::Path;

use anyhow::Result;
#[cfg(not(unix))]
use anyhow::bail;

#[cfg(unix)]
mod unix {
    use std::ffi::OsString;
    use std::fs::{File, Metadata};
    use std::io::{Read, Write};
    use std::os::unix::fs::{FileExt, MetadataExt};
    use std::path::{Component, Path, PathBuf};

    use anyhow::{Context, Result, bail};
    use rustix::fd::OwnedFd;
    use rustix::fs::{self, AtFlags, FileType, Mode, OFlags};
    use rustix::io::Errno;

    pub(super) struct Directory {
        fd: OwnedFd,
        path: PathBuf,
    }

    pub(super) struct ExistingFile {
        file: File,
        pub(super) metadata: Metadata,
        pub(super) content: String,
        pub(super) bytes: Vec<u8>,
    }

    struct StagedFile<'a> {
        directory: StagedDirectory<'a>,
        file: File,
        content: Vec<u8>,
        mode: u32,
    }

    struct StagedDirectory<'a> {
        parent: &'a Directory,
        name: OsString,
        fd: OwnedFd,
    }

    fn directory_flags() -> OFlags {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let access = OFlags::PATH;
        #[cfg(target_os = "macos")]
        let access = OFlags::from_bits_retain(libc::O_SEARCH as _);
        #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
        let access = OFlags::RDONLY;
        access | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
    }

    impl Directory {
        pub(super) fn open(path: &Path, create: bool) -> Result<Option<Self>> {
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                std::env::current_dir()?.join(path)
            };
            let flags = directory_flags();
            let mut fd = fs::open("/", flags, Mode::empty())?;
            let mut walked = PathBuf::from("/");
            for component in path.components() {
                let part = match component {
                    Component::RootDir | Component::CurDir => continue,
                    Component::Normal(part) => part,
                    Component::ParentDir => std::ffi::OsStr::new(".."),
                    Component::Prefix(_) => bail!("unsupported hook directory: {}", path.display()),
                };
                walked.push(part);
                let mut next = fs::openat(&fd, part, flags, Mode::empty());
                if matches!(next, Err(Errno::NOENT)) && create {
                    match fs::mkdirat(&fd, part, Mode::from_raw_mode(0o755)) {
                        Ok(()) | Err(Errno::EXIST) => {}
                        Err(error) => {
                            return Err(error).with_context(|| {
                                format!("creating hook directory {}", walked.display())
                            });
                        }
                    }
                    next = fs::openat(&fd, part, flags, Mode::empty());
                }
                match next {
                    Ok(next) => fd = next,
                    Err(Errno::NOENT) => return Ok(None),
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!(
                                "refusing unsafe hook directory component {}",
                                walked.display()
                            )
                        });
                    }
                }
            }
            Ok(Some(Self { fd, path }))
        }

        pub(super) fn same_directory(&self, other: &Self) -> Result<bool> {
            let a = fs::fstat(&self.fd)?;
            let b = fs::fstat(&other.fd)?;
            Ok(a.st_dev == b.st_dev && a.st_ino == b.st_ino)
        }

        pub(super) fn has_regular_file(&self, name: &str) -> Result<bool> {
            match fs::statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) => Ok(FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile),
                Err(Errno::NOENT) => Ok(false),
                Err(error) => Err(error)
                    .with_context(|| format!("checking {}", self.path.join(name).display())),
            }
        }

        pub(super) fn read(&self, name: &str) -> Result<Option<ExistingFile>> {
            let fd = match fs::openat(
                &self.fd,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(fd) => fd,
                Err(Errno::NOENT) => return Ok(None),
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "refusing unsafe hook file {}",
                            self.path.join(name).display()
                        )
                    });
                }
            };
            let mut file = File::from(fd);
            let metadata = file.metadata()?;
            if !metadata.is_file() {
                bail!(
                    "refusing non-regular hook file {}",
                    self.path.join(name).display()
                );
            }
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            let content = std::str::from_utf8(&bytes).unwrap_or_default().to_owned();
            Ok(Some(ExistingFile {
                file,
                metadata,
                content,
                bytes,
            }))
        }

        pub(super) fn write(
            &self,
            name: &str,
            content: &[u8],
            mode: u32,
            existing: Option<&ExistingFile>,
        ) -> Result<()> {
            self.check_unchanged(name, existing)?;
            if existing
                .is_some_and(|file| file.bytes == content && file.metadata.mode() & 0o7777 == mode)
            {
                return Ok(());
            }
            let staged = self.stage(content, mode)?;
            self.publish(name, existing, &staged)
        }

        fn stage(&self, content: &[u8], mode: u32) -> Result<StagedFile<'_>> {
            // tempfile supplies fresh names; creation and cleanup stay relative to pinned descriptors.
            let temporary = tempfile::Builder::new()
                .prefix(".codesage-hook-")
                .disable_cleanup(true)
                .make_in(&self.path, |path| {
                    let name = path.file_name().expect("tempfile supplies a leaf name");
                    fs::mkdirat(&self.fd, name, Mode::from_raw_mode(0o700))?;
                    fs::openat(&self.fd, name, directory_flags(), Mode::empty()).map_err(Into::into)
                })?;
            let (fd, path) = temporary.into_parts();
            let directory = StagedDirectory {
                parent: self,
                name: path
                    .file_name()
                    .expect("tempfile supplies a leaf name")
                    .to_owned(),
                fd,
            };
            let mut file = File::from(fs::openat(
                &directory.fd,
                "hook",
                OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )?);
            file.write_all(content)?;
            fs::fchmod(&file, Mode::from_raw_mode(mode as _))?;
            Ok(StagedFile {
                directory,
                file,
                content: content.to_vec(),
                mode,
            })
        }

        fn check_unchanged(&self, name: &str, existing: Option<&ExistingFile>) -> Result<()> {
            let stat = match fs::statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) => Some(stat),
                Err(Errno::NOENT) => None,
                Err(error) => return Err(error.into()),
            };
            let matches = match (existing, stat) {
                (None, None) => true,
                (Some(existing), Some(stat)) => {
                    let held = existing.file.metadata()?;
                    let identity = fs::fstat(&existing.file)?;
                    FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile
                        && identity.st_dev == stat.st_dev
                        && identity.st_ino == stat.st_ino
                        && held.len() == existing.metadata.len()
                        && held.modified()? == existing.metadata.modified()?
                }
                _ => false,
            };
            if !matches {
                bail!(
                    "hook file changed during installation: {}",
                    self.path.join(name).display()
                );
            }
            Ok(())
        }

        fn publish(
            &self,
            name: &str,
            existing: Option<&ExistingFile>,
            staged: &StagedFile<'_>,
        ) -> Result<()> {
            staged.check_source()?;
            self.check_unchanged(name, existing)?;
            self.publish_checked(name, existing, staged)
        }

        fn publish_checked(
            &self,
            name: &str,
            existing: Option<&ExistingFile>,
            staged: &StagedFile<'_>,
        ) -> Result<()> {
            if existing.is_some() {
                // renameat never follows a replacement leaf; Unix has no inode-conditioned rename.
                fs::renameat(&staged.directory.fd, "hook", &self.fd, name)?;
            } else {
                fs::linkat(
                    &staged.directory.fd,
                    "hook",
                    &self.fd,
                    name,
                    AtFlags::empty(),
                )?;
            }
            staged.check_published(self, name)
        }
    }

    impl StagedFile<'_> {
        fn matches_path(&self, parent: &OwnedFd, name: &str) -> Result<bool> {
            let before = self.file.metadata()?;
            if !before.is_file()
                || before.len() != self.content.len() as u64
                || before.mode() & 0o7777 != self.mode
            {
                return Ok(false);
            }
            let mut content = vec![0; self.content.len()];
            self.file.read_exact_at(&mut content, 0)?;
            let after = self.file.metadata()?;
            if content != self.content
                || before.len() != after.len()
                || before.modified()? != after.modified()?
                || after.mode() & 0o7777 != self.mode
            {
                return Ok(false);
            }
            let path = match fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(path) => path,
                Err(Errno::NOENT) => return Ok(false),
                Err(error) => return Err(error.into()),
            };
            let held = fs::fstat(&self.file)?;
            Ok(
                FileType::from_raw_mode(path.st_mode) == FileType::RegularFile
                    && path.st_dev == held.st_dev
                    && path.st_ino == held.st_ino,
            )
        }

        fn check_source(&self) -> Result<()> {
            if !self
                .matches_path(&self.directory.fd, "hook")
                .context("checking staged hook file")?
            {
                bail!("staged hook file changed during installation");
            }
            Ok(())
        }

        fn check_published(&self, directory: &Directory, name: &str) -> Result<()> {
            let message = || {
                format!(
                    "hook publication is inconsistent: {}",
                    directory.path.join(name).display()
                )
            };
            if !self
                .matches_path(&directory.fd, name)
                .with_context(message)?
            {
                // Unix cannot conditionally unlink an inode; leave a replaced leaf untouched.
                bail!("{}", message());
            }
            Ok(())
        }
    }

    impl Drop for StagedDirectory<'_> {
        fn drop(&mut self) {
            let _ = fs::unlinkat(&self.fd, "hook", AtFlags::empty());
            let _ = fs::unlinkat(&self.parent.fd, &self.name, AtFlags::REMOVEDIR);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::{PermissionsExt, symlink};

        fn tempdir() -> tempfile::TempDir {
            tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap()
        }

        fn staged_path(root: &Path) -> PathBuf {
            std::fs::read_dir(root)
                .unwrap()
                .map(|entry| entry.unwrap())
                .find(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".codesage-hook-")
                })
                .unwrap()
                .path()
                .join("hook")
        }

        #[test]
        fn staged_source_links_are_refused_before_publication() {
            for existing in [false, true] {
                let root = tempdir();
                let directory = Directory::open(root.path(), false).unwrap().unwrap();
                let hook = root.path().join("post-commit");
                let target = root.path().join("target");
                std::fs::write(&target, b"preserve target\n").unwrap();
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
                if existing {
                    std::fs::write(&hook, b"codesage install-hooks\nold\n").unwrap();
                }
                let prior = directory.read("post-commit").unwrap();
                let staged = directory.stage(b"new\n", 0o755).unwrap();
                let source = staged_path(root.path());
                std::fs::remove_file(&source).unwrap();
                symlink(&target, &source).unwrap();

                assert!(
                    directory
                        .publish("post-commit", prior.as_ref(), &staged)
                        .is_err()
                );
                assert_eq!(std::fs::read_link(&source).unwrap(), target);
                if existing {
                    assert_eq!(
                        std::fs::read(&hook).unwrap(),
                        b"codesage install-hooks\nold\n"
                    );
                } else {
                    assert!(!hook.exists());
                }
                drop(staged);

                assert_eq!(std::fs::read(&target).unwrap(), b"preserve target\n");
                assert_eq!(std::fs::metadata(&target).unwrap().mode() & 0o777, 0o640);
                assert!(!source.exists());
            }
        }

        #[test]
        fn source_links_published_after_validation_do_not_report_success() {
            for name in [
                "post-commit",
                "post-merge",
                "post-checkout",
                "post-rewrite",
                "pre-commit",
            ] {
                for existing in [false, true] {
                    for live in [false, true] {
                        let root = tempdir();
                        let directory = Directory::open(root.path(), false).unwrap().unwrap();
                        let hook = root.path().join(name);
                        let target = root.path().join("target");
                        if live {
                            std::fs::write(&target, b"preserve target\n").unwrap();
                            std::fs::set_permissions(
                                &target,
                                std::fs::Permissions::from_mode(0o640),
                            )
                            .unwrap();
                        }
                        if existing {
                            std::fs::write(&hook, b"codesage install-hooks\nold\n").unwrap();
                        }
                        let prior = directory.read(name).unwrap();
                        let staged = directory.stage(b"new\n", 0o755).unwrap();
                        let source = staged_path(root.path());
                        staged.check_source().unwrap();
                        directory.check_unchanged(name, prior.as_ref()).unwrap();
                        std::fs::remove_file(&source).unwrap();
                        symlink(&target, &source).unwrap();

                        let error = directory
                            .publish_checked(name, prior.as_ref(), &staged)
                            .unwrap_err();

                        assert!(
                            error
                                .to_string()
                                .contains("hook publication is inconsistent")
                        );
                        assert_eq!(std::fs::read_link(&hook).unwrap(), target);
                        drop(staged);
                        assert_eq!(std::fs::read_link(&hook).unwrap(), target);
                        if live {
                            assert_eq!(std::fs::read(&target).unwrap(), b"preserve target\n");
                            assert_eq!(std::fs::metadata(&target).unwrap().mode() & 0o777, 0o640);
                        } else {
                            assert!(!target.exists());
                        }
                        assert!(std::fs::symlink_metadata(&source).is_err());
                    }
                }
            }
        }

        #[test]
        fn regular_source_substitutions_are_detected_before_and_after_validation() {
            for after_validation in [false, true] {
                for existing in [false, true] {
                    let root = tempdir();
                    let directory = Directory::open(root.path(), false).unwrap().unwrap();
                    let hook = root.path().join("post-commit");
                    if existing {
                        std::fs::write(&hook, b"codesage install-hooks\nold\n").unwrap();
                    }
                    let prior = directory.read("post-commit").unwrap();
                    let staged = directory.stage(b"new\n", 0o755).unwrap();
                    let source = staged_path(root.path());
                    let replacement = root.path().join("replacement");
                    std::fs::write(&replacement, b"new\n").unwrap();
                    std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o755))
                        .unwrap();
                    let replacement_inode = std::fs::metadata(&replacement).unwrap().ino();
                    staged.check_source().unwrap();
                    directory
                        .check_unchanged("post-commit", prior.as_ref())
                        .unwrap();
                    std::fs::rename(&replacement, &source).unwrap();

                    let result = if after_validation {
                        directory.publish_checked("post-commit", prior.as_ref(), &staged)
                    } else {
                        directory.publish("post-commit", prior.as_ref(), &staged)
                    };

                    assert!(result.is_err());
                    if after_validation {
                        assert_eq!(std::fs::metadata(&hook).unwrap().ino(), replacement_inode);
                        assert_eq!(std::fs::read(&hook).unwrap(), b"new\n");
                        assert_eq!(std::fs::metadata(&hook).unwrap().mode() & 0o777, 0o755);
                    } else if existing {
                        assert_eq!(
                            std::fs::read(&hook).unwrap(),
                            b"codesage install-hooks\nold\n"
                        );
                    } else {
                        assert!(!hook.exists());
                    }
                    drop(staged);
                    assert!(std::fs::symlink_metadata(&source).is_err());
                }
            }
        }

        #[test]
        fn staged_payload_or_mode_mutations_do_not_report_success() {
            for change_mode in [false, true] {
                for existing in [false, true] {
                    let root = tempdir();
                    let directory = Directory::open(root.path(), false).unwrap().unwrap();
                    let hook = root.path().join("post-commit");
                    if existing {
                        std::fs::write(&hook, b"codesage install-hooks\nold\n").unwrap();
                    }
                    let prior = directory.read("post-commit").unwrap();
                    let staged = directory.stage(b"new\n", 0o755).unwrap();
                    let source = staged_path(root.path());
                    staged.check_source().unwrap();
                    directory
                        .check_unchanged("post-commit", prior.as_ref())
                        .unwrap();
                    if change_mode {
                        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o640))
                            .unwrap();
                    } else {
                        std::fs::write(&source, b"bad\n").unwrap();
                    }

                    let error = directory
                        .publish_checked("post-commit", prior.as_ref(), &staged)
                        .unwrap_err();

                    assert!(
                        error
                            .to_string()
                            .contains("hook publication is inconsistent")
                    );
                    assert!(std::fs::symlink_metadata(&hook).unwrap().is_file());
                    assert_eq!(
                        std::fs::read(&hook).unwrap(),
                        if change_mode { b"new\n" } else { b"bad\n" }
                    );
                    assert_eq!(
                        std::fs::metadata(&hook).unwrap().mode() & 0o777,
                        if change_mode { 0o640 } else { 0o755 }
                    );
                    drop(staged);
                    assert!(std::fs::symlink_metadata(&source).is_err());
                }
            }
        }

        #[test]
        fn replacing_a_pinned_parent_cannot_redirect_reads_or_writes() {
            let root = tempdir();
            let hooks = root.path().join("hooks");
            let retained = root.path().join("retained");
            let target = root.path().join("target");
            std::fs::create_dir(&hooks).unwrap();
            std::fs::create_dir(&target).unwrap();
            std::fs::write(hooks.join("post-commit"), b"codesage install-hooks\nold\n").unwrap();
            let directory = Directory::open(&hooks, false).unwrap().unwrap();
            std::fs::rename(&hooks, &retained).unwrap();
            symlink(&target, &hooks).unwrap();
            let existing = directory.read("post-commit").unwrap().unwrap();
            directory
                .write("post-commit", b"new\n", 0o755, Some(&existing))
                .unwrap();
            assert_eq!(
                std::fs::read(retained.join("post-commit")).unwrap(),
                b"new\n"
            );
            assert_eq!(std::fs::read_link(&hooks).unwrap(), target);
            assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
            assert_eq!(std::fs::read_dir(&retained).unwrap().count(), 1);
        }

        #[test]
        fn a_link_inserted_before_existing_hook_revalidation_survives() {
            for name in [
                "post-commit",
                "post-merge",
                "post-checkout",
                "post-rewrite",
                "pre-commit",
            ] {
                let root = tempdir();
                let directory = Directory::open(root.path(), false).unwrap().unwrap();
                let hook = root.path().join(name);
                let target = root.path().join("target");
                let marker = format!("target {}\n", root.path().display());
                std::fs::write(&target, &marker).unwrap();
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
                std::fs::write(&hook, b"codesage install-hooks\nold\n").unwrap();
                let existing = directory.read(name).unwrap().unwrap();
                let staged = directory.stage(b"new\n", 0o755).unwrap();
                std::fs::remove_file(&hook).unwrap();
                symlink(&target, &hook).unwrap();

                assert!(directory.publish(name, Some(&existing), &staged).is_err());
                drop(staged);

                assert_eq!(std::fs::read_link(&hook).unwrap(), target);
                assert_eq!(std::fs::read_to_string(&target).unwrap(), marker);
                assert_eq!(std::fs::metadata(&target).unwrap().mode() & 0o777, 0o640);
                assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 2);
            }
        }

        #[test]
        fn a_link_inserted_after_absent_hook_revalidation_survives() {
            let root = tempdir();
            let directory = Directory::open(root.path(), false).unwrap().unwrap();
            let staged = directory.stage(b"new\n", 0o755).unwrap();
            directory.check_unchanged("post-commit", None).unwrap();
            let target = root.path().join("absent-target");
            let hook = root.path().join("post-commit");
            symlink(&target, &hook).unwrap();

            assert!(
                directory
                    .publish_checked("post-commit", None, &staged)
                    .is_err()
            );
            drop(staged);

            assert_eq!(std::fs::read_link(&hook).unwrap(), target);
            assert!(!target.exists());
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
        }

        #[test]
        fn replacing_an_owned_hardlink_preserves_the_other_inode_name() {
            let root = tempdir();
            let directory = Directory::open(root.path(), false).unwrap().unwrap();
            let target = root.path().join("target");
            let hook = root.path().join("post-commit");
            let old = b"codesage install-hooks\nold\n";
            std::fs::write(&target, old).unwrap();
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
            std::fs::hard_link(&target, &hook).unwrap();
            let existing = directory.read("post-commit").unwrap().unwrap();

            directory
                .write("post-commit", b"new\n", 0o755, Some(&existing))
                .unwrap();

            assert_eq!(std::fs::read(&hook).unwrap(), b"new\n");
            assert_eq!(std::fs::read(&target).unwrap(), old);
            assert_eq!(std::fs::metadata(&target).unwrap().mode() & 0o777, 0o640);
            assert_ne!(
                std::fs::metadata(hook).unwrap().ino(),
                std::fs::metadata(target).unwrap().ino()
            );
        }
    }
}

#[cfg(unix)]
use unix::{Directory, ExistingFile};

#[cfg(not(unix))]
struct Directory;
#[cfg(not(unix))]
struct ExistingFile {
    content: String,
    bytes: Vec<u8>,
}

pub(super) struct HookDirectory(Directory);

pub(super) struct HookFile {
    inner: ExistingFile,
}

impl HookFile {
    pub(super) fn content(&self) -> &str {
        &self.inner.content
    }

    pub(super) fn bytes(&self) -> &[u8] {
        &self.inner.bytes
    }

    pub(super) fn mode(&self) -> u32 {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            self.inner.metadata.mode() & 0o7777
        }
        #[cfg(not(unix))]
        0o644
    }
}

impl HookDirectory {
    pub(super) fn open(path: &Path, create: bool) -> Result<Option<Self>> {
        #[cfg(unix)]
        {
            Directory::open(path, create).map(|directory| directory.map(Self))
        }
        #[cfg(not(unix))]
        {
            let _ = (path, create);
            bail!("secure hook installation requires Unix filesystem operations");
        }
    }

    pub(super) fn create(path: &Path) -> Result<Self> {
        Self::open(path, true)?
            .ok_or_else(|| anyhow::anyhow!("hook directory disappeared: {}", path.display()))
    }

    pub(super) fn same_directory(&self, other: &Self) -> Result<bool> {
        #[cfg(unix)]
        {
            self.0.same_directory(&other.0)
        }
        #[cfg(not(unix))]
        {
            let _ = other;
            bail!("secure hook installation requires Unix filesystem operations");
        }
    }

    pub(super) fn has_regular_file(&self, name: &str) -> Result<bool> {
        #[cfg(unix)]
        {
            self.0.has_regular_file(name)
        }
        #[cfg(not(unix))]
        {
            let _ = name;
            bail!("secure hook installation requires Unix filesystem operations");
        }
    }

    pub(super) fn read(&self, name: &str) -> Result<Option<HookFile>> {
        #[cfg(unix)]
        {
            self.0
                .read(name)
                .map(|file| file.map(|inner| HookFile { inner }))
        }
        #[cfg(not(unix))]
        {
            let _ = name;
            bail!("secure hook installation requires Unix filesystem operations");
        }
    }

    pub(super) fn write(
        &self,
        name: &str,
        content: &[u8],
        mode: u32,
        existing: Option<&HookFile>,
    ) -> Result<()> {
        #[cfg(unix)]
        {
            self.0
                .write(name, content, mode, existing.map(|file| &file.inner))
        }
        #[cfg(not(unix))]
        {
            let _ = (name, content, mode, existing);
            bail!("secure hook installation requires Unix filesystem operations");
        }
    }
}
