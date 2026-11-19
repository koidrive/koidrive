// SPDX-License-Identifier: Apache-2.0

#[cfg(any(test, feature = "test-hooks"))]
use std::cell::Cell;
use std::ffi::{OsStr, OsString};
use std::fs::{DirBuilder, File};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::Path;

use rustix::fs::{self, AtFlags, Dir, FileType, FlockOperation, Mode, OFlags, flock};

mod flock;
pub use flock::Flock;

const PREFIX: &str = ".koi-state-";
const SUFFIX: &str = ".tmp";
const RANDOM_BYTES: usize = 16;

/// Whether publication replaces an existing file or fails if one exists.
#[derive(Clone, Copy, Debug)]
pub enum Publish {
    Replace,
    Create,
}

/// A state file whose new contents every reader already sees. `durable` holds the error of a step after
/// publication that failed, such as the parent directory's sync; until that is `Ok`, a crash can bring back the old
/// contents.
#[must_use = "the new contents are in place even when `durable` holds an error"]
#[derive(Debug)]
pub struct Published<T = ()> {
    pub value: T,
    pub durable: io::Result<()>,
}

impl<T> Published<T> {
    /// The value, after logging a step after publication that failed: every reader already sees the new contents.
    pub fn warn_unless_durable(self, path: &Path) -> T {
        if let Err(error) = self.durable {
            tracing::warn!(path = %path.display(), %error, "a state change is in place, but it may not survive a crash");
        }
        self.value
    }
}

/// A private, same-directory stage whose inode lock distinguishes live writes from abandoned writes.
/// Dropping it removes the stage. After a process dies, [`recover`] removes its abandoned stages.
pub struct AtomicFile {
    file: Flock,
    directory: File,
    stage: Option<OsString>,
    destination: OsString,
}

impl AtomicFile {
    /// Creates a mode-0600 stage with exclusive, no-follow creation. The parent must exist.
    /// The directory descriptor anchors publication and cleanup even if an ancestor moves.
    pub fn prepare(path: &Path) -> io::Result<Self> {
        let destination = path
            .file_name()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "a state file needs a name")
            })?
            .to_owned();
        if is_reserved_stage(destination.as_bytes()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a state-file destination cannot use the staging namespace",
            ));
        }
        let directory = open_directory(parent(path))?;
        let lock = DirectoryLock::new(&directory)?;
        let recovery = recover_locked(&directory)?;
        if let Some(stage) = aliased_stage(&directory, &destination)? {
            return Err(recovery.alias_error(
                &stage,
                "a state-file destination cannot alias a helper stage",
            ));
        }
        for _ in 0..16 {
            let mut random = [0; RANDOM_BYTES];
            getrandom::fill(&mut random).map_err(io::Error::other)?;
            let random: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
            let stage = OsString::from(format!("{PREFIX}{random}{SUFFIX}"));
            let file = match fs::openat(
                &directory,
                &stage,
                OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            ) {
                Ok(file) => File::from(file),
                Err(rustix::io::Errno::EXIST) => continue,
                Err(error) => return Err(error.into()),
            };
            let file = match Flock::wait(file) {
                Ok(file) => file,
                Err(error) => {
                    let _ = fs::unlinkat(&directory, &stage, AtFlags::empty());
                    return Err(error);
                }
            };
            drop(lock);
            return Ok(Self {
                file,
                directory,
                stage: Some(stage),
                destination,
            });
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique state-file stage",
        ))
    }

    /// The stage, for writing bytes and setting permissions or backup exclusions before publication.
    pub fn as_file(&self) -> &File {
        &self.file
    }

    /// Syncs the contents, publishes atomically, and syncs the parent directory.
    /// `Create` never replaces an existing destination. An error leaves the destination as it was.
    pub fn finish(self, publish: Publish) -> io::Result<Published<File>> {
        self.finish_with_legacy(publish, &[])
    }

    /// Selects obsolete regular files and symlinks from the anchored directory, then publishes and removes them.
    /// Callers must own the selected names and exclude concurrent writers of those legacy stages.
    /// Cleanup follows the error contract of [`Self::finish_with_legacy`].
    pub fn finish_with_legacy_matching(
        self,
        publish: Publish,
        legacy: impl FnMut(&OsStr) -> bool,
    ) -> io::Result<Published<File>> {
        let names = matching_legacy(&self.directory, legacy)?;
        self.finish_with_legacy(
            publish,
            &names.iter().map(OsString::as_os_str).collect::<Vec<_>>(),
        )
    }

    /// Publishes the file, then removes obsolete sibling stages while the directory is locked.
    /// Callers must own the legacy names. Names cannot escape the directory, name the destination,
    /// or overlap the helper's staging namespace. Permission-denied obsolete stages are left untouched.
    /// Other cleanup failures follow publication, in [`Published::durable`].
    pub fn finish_with_legacy(
        mut self,
        publish: Publish,
        legacy: &[&OsStr],
    ) -> io::Result<Published<File>> {
        validate_legacy(&self.destination, legacy)?;
        self.file.sync_all()?;
        let file = self.file.try_clone()?;
        let _lock = DirectoryLock::new(&self.directory)?;
        if aliased_stage(&self.directory, &self.destination)?.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a state-file destination cannot alias a helper stage",
            ));
        }
        for name in legacy {
            if aliased_stage(&self.directory, name)?.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a legacy name cannot alias a helper stage",
                ));
            }
        }
        let stage = self.stage.as_ref().unwrap();
        if !same_inode(&self.directory, stage, &self.file)? {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "the state-file stage was replaced",
            ));
        }
        match publish {
            Publish::Replace => {
                fs::renameat(&self.directory, stage, &self.directory, &self.destination)?
            }
            Publish::Create => fs::linkat(
                &self.directory,
                stage,
                &self.directory,
                &self.destination,
                AtFlags::empty(),
            )?,
        }
        let unstaged = match publish {
            Publish::Replace => Ok(()),
            Publish::Create => fs::unlinkat(&self.directory, stage, AtFlags::empty()),
        };
        if unstaged.is_ok() {
            self.stage = None;
        }
        let durable = unstaged.map_err(io::Error::from).and_then(|()| {
            for name in legacy {
                if same_inode(&self.directory, name, &self.file)? {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "a legacy name cannot alias the published state file",
                    ));
                }
            }
            for name in legacy {
                unlink_obsolete(&self.directory, name)?;
            }
            sync_published(&self.directory)
        });
        Ok(Published {
            value: file,
            durable,
        })
    }
}

impl Drop for AtomicFile {
    fn drop(&mut self) {
        if let Some(stage) = &self.stage
            && let Ok(_lock) = DirectoryLock::new(&self.directory)
            && same_inode(&self.directory, stage, &self.file).unwrap_or(false)
        {
            let _ = fs::unlinkat(&self.directory, stage, AtFlags::empty());
        }
    }
}

/// Syncs a file or folder whose new contents every reader can already see.
pub fn sync_published(file: &File) -> io::Result<()> {
    #[cfg(any(test, feature = "test-hooks"))]
    if FAILING_SYNCS.get() {
        return Err(io::Error::other("injected sync failure after publication"));
    }
    file.sync_all()
}

#[cfg(any(test, feature = "test-hooks"))]
thread_local! {
    static FAILING_SYNCS: Cell<bool> = const { Cell::new(false) };
}

/// Makes every [`sync_published`] on this thread fail until the guard is dropped.
#[cfg(any(test, feature = "test-hooks"))]
pub fn fail_syncs_after_publication() -> FailingSyncs {
    FAILING_SYNCS.set(true);
    FailingSyncs
}

#[cfg(any(test, feature = "test-hooks"))]
pub struct FailingSyncs;

#[cfg(any(test, feature = "test-hooks"))]
impl Drop for FailingSyncs {
    fn drop(&mut self) {
        FAILING_SYNCS.set(false);
    }
}

/// Replaces a state file with complete, synced bytes using [`AtomicFile`].
pub fn write(path: &Path, bytes: &[u8]) -> io::Result<Published> {
    write_with_legacy(path, bytes, &[])
}

/// Renames `from` to `to`, then syncs the folder of `to`. An error leaves both names as they were.
pub fn rename(from: &Path, to: &Path) -> io::Result<Published> {
    std::fs::rename(from, to)?;
    Ok(Published {
        value: (),
        durable: open_directory(parent(to)).and_then(|dir| sync_published(&dir)),
    })
}

/// Removes the file at `path`, then syncs its folder. An error leaves the file as it was.
pub fn remove(path: &Path) -> io::Result<Published> {
    std::fs::remove_file(path)?;
    Ok(Published {
        value: (),
        durable: open_directory(parent(path)).and_then(|dir| sync_published(&dir)),
    })
}

/// The file in which Koidrive records that it created a state folder.
pub const MADE: &str = ".koidrive-state";

/// Creates `dir`, which is `root` or a folder inside it, and any missing folders on the way as private folders. When
/// this call creates `root` itself, it records in `root` that Koidrive made it; see [`made`].
pub fn create_dir(root: &Path, dir: &Path) -> io::Result<()> {
    let root = match root.as_os_str().is_empty() {
        true => Path::new("."),
        false => root,
    };
    if let Some(parent) = root
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    match DirBuilder::new().mode(0o700).create(root) {
        Ok(()) => {
            let recorded = std::fs::metadata(root)
                .and_then(|folder| write(&root.join(MADE), made_record(&folder).as_bytes()));
            match recorded {
                Ok(published) => published.warn_unless_durable(&root.join(MADE)),
                Err(error) => {
                    let _ = std::fs::remove_dir(root);
                    return Err(error);
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

/// Whether the folder at `root` is one [`create_dir`] made: it holds the record of its creation, and that record
/// names this folder rather than one it was copied from.
pub fn made(root: &Path) -> bool {
    let record = root.join(MADE);
    std::fs::symlink_metadata(&record).is_ok_and(|meta| meta.is_file())
        && match (std::fs::metadata(root), File::open(&record)) {
            (Ok(folder), Ok(file)) => {
                let expected = made_record(&folder);
                let mut text = String::new();
                file.take(expected.len() as u64 + 1)
                    .read_to_string(&mut text)
                    .is_ok_and(|_| text == expected)
            }
            _ => false,
        }
}

fn made_record(folder: &std::fs::Metadata) -> String {
    format!("{} {}\n", folder.dev(), folder.ino())
}

/// Replaces a state file and removes obsolete sibling stages owned by the caller.
/// Permission-denied obsolete stages are left untouched; other cleanup failures follow publication.
pub fn write_with_legacy(path: &Path, bytes: &[u8], legacy: &[&OsStr]) -> io::Result<Published> {
    let stage = AtomicFile::prepare(path)?;
    stage.as_file().write_all(bytes)?;
    let published = stage.finish_with_legacy(Publish::Replace, legacy)?;
    Ok(Published {
        value: (),
        durable: published.durable,
    })
}

/// Removes a state file and obsolete sibling stages, then syncs the directory without allocating a stage.
/// Selects obsolete regular files and symlinks from the anchored directory. Callers must own the selected
/// names and exclude concurrent writers of the destination and legacy stages.
/// Missing files or directories are already removed. Destination removal errors are returned.
/// Permission-denied obsolete stages are left untouched; other cleanup failures follow destination removal.
pub fn remove_with_legacy_matching(
    path: &Path,
    legacy: impl FnMut(&OsStr) -> bool,
) -> io::Result<Published> {
    let destination = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a state file needs a name"))?;
    if is_reserved_stage(destination.as_bytes()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a state-file destination cannot use the staging namespace",
        ));
    }
    let directory = match open_directory(parent(path)) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Published {
                value: (),
                durable: Ok(()),
            });
        }
        Err(error) => return Err(error),
    };
    let _lock = DirectoryLock::new(&directory)?;
    let recovery = recover_locked(&directory)?;
    let names = matching_legacy(&directory, legacy)?;
    let legacy: Vec<_> = names.iter().map(OsString::as_os_str).collect();
    validate_legacy(destination, &legacy)?;
    for name in std::iter::once(&destination).chain(&legacy) {
        if let Some(stage) = aliased_stage(&directory, name)? {
            return Err(recovery.alias_error(&stage, "a removed name cannot alias a helper stage"));
        }
    }
    match fs::unlinkat(&directory, destination, AtFlags::empty()) {
        Ok(()) | Err(rustix::io::Errno::NOENT) => {}
        Err(error) => return Err(error.into()),
    }
    let durable = (legacy.iter())
        .try_for_each(|name| unlink_obsolete(&directory, name).map(drop))
        .and_then(|()| sync_published(&directory));
    Ok(Published { value: (), durable })
}

/// Removes this helper's abandoned stages, but never a stage locked by a live writer.
/// Inaccessible stages are left untouched so they cannot block other publications.
/// Call before disclosing a directory's contents. A missing directory needs no recovery.
/// Returns the number of removed stages so inventory can recognize otherwise empty state directories.
pub fn recover(dir: &Path) -> io::Result<usize> {
    let directory = match open_directory(dir) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let _lock = DirectoryLock::new(&directory)?;
    let recovery = recover_locked(&directory)?;
    directory.sync_all()?;
    Ok(recovery.removed)
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn validate_legacy(destination: &OsStr, legacy: &[&OsStr]) -> io::Result<()> {
    for name in legacy {
        if Path::new(name).file_name() != Some(*name)
            || name.as_bytes().contains(&0)
            || name.as_bytes().eq_ignore_ascii_case(destination.as_bytes())
            || is_reserved_stage(name.as_bytes())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid legacy stage name",
            ));
        }
    }
    Ok(())
}

fn matching_legacy(
    directory: &File,
    mut legacy: impl FnMut(&OsStr) -> bool,
) -> io::Result<Vec<OsString>> {
    let mut names = Vec::new();
    for entry in Dir::read_from(directory)? {
        let entry = entry?;
        let name = OsStr::from_bytes(entry.file_name().to_bytes());
        if !legacy(name) {
            continue;
        }
        let metadata = match fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(metadata) => metadata,
            Err(rustix::io::Errno::NOENT) => continue,
            Err(error) => return Err(error.into()),
        };
        if matches!(
            FileType::from_raw_mode(metadata.st_mode),
            FileType::RegularFile | FileType::Symlink
        ) {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

fn open_directory(dir: &Path) -> io::Result<File> {
    Ok(File::from(fs::open(
        dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?))
}

struct DirectoryLock<'a>(&'a File);

impl<'a> DirectoryLock<'a> {
    fn new(directory: &'a File) -> io::Result<Self> {
        flock(directory, FlockOperation::LockExclusive)?;
        Ok(Self(directory))
    }
}

impl Drop for DirectoryLock<'_> {
    fn drop(&mut self) {
        let _ = flock(self.0, FlockOperation::Unlock);
    }
}

fn same_inode(directory: &File, name: &std::ffi::OsStr, file: &File) -> io::Result<bool> {
    let named = match fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(named) => named,
        Err(rustix::io::Errno::NOENT) => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let opened = fs::fstat(file)?;
    Ok((named.st_dev, named.st_ino) == (opened.st_dev, opened.st_ino))
}

fn aliased_stage(directory: &File, name: &OsStr) -> io::Result<Option<OsString>> {
    let named = match fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(named) => named,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    for entry in Dir::read_from(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        if !is_reserved_stage(name.to_bytes()) {
            continue;
        }
        let stage = match fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stage) => stage,
            Err(rustix::io::Errno::NOENT) => continue,
            Err(error) => return Err(error.into()),
        };
        if (named.st_dev, named.st_ino) == (stage.st_dev, stage.st_ino) {
            return Ok(Some(OsStr::from_bytes(name.to_bytes()).to_owned()));
        }
    }
    Ok(None)
}

fn is_reserved_stage(name: &[u8]) -> bool {
    name.len() == PREFIX.len() + RANDOM_BYTES * 2 + SUFFIX.len()
        && name[..PREFIX.len()].eq_ignore_ascii_case(PREFIX.as_bytes())
        && name[name.len() - SUFFIX.len()..].eq_ignore_ascii_case(SUFFIX.as_bytes())
        && name[PREFIX.len()..name.len() - SUFFIX.len()]
            .iter()
            .all(|byte| byte.is_ascii_hexdigit())
}

fn is_stage(name: &[u8]) -> bool {
    name.strip_prefix(PREFIX.as_bytes())
        .and_then(|name| name.strip_suffix(SUFFIX.as_bytes()))
        .is_some_and(|random| {
            random.len() == RANDOM_BYTES * 2 && random.iter().all(|byte| byte.is_ascii_hexdigit())
        })
}

fn unlink_obsolete(directory: &File, name: &OsStr) -> io::Result<bool> {
    match fs::unlinkat(directory, name, AtFlags::empty()) {
        Ok(()) => Ok(true),
        Err(rustix::io::Errno::NOENT | rustix::io::Errno::ACCESS | rustix::io::Errno::PERM) => {
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}

struct Recovery {
    removed: usize,
    left: Vec<(OsString, Left)>,
}

enum Left {
    Held,
    Failed(io::Error),
}

enum Recovered {
    Removed,
    Held,
    Untouched,
}

impl Recovery {
    fn alias_error(&self, stage: &OsStr, problem: &str) -> io::Error {
        match self.left.iter().find(|(name, _)| name == stage) {
            Some((_, Left::Held)) => io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{problem}: a live writer holds that stage"),
            ),
            Some((_, Left::Failed(error))) => io::Error::new(
                error.kind(),
                format!("{problem}: recovery could not clear that stage: {error}"),
            ),
            None => io::Error::new(io::ErrorKind::InvalidInput, problem.to_owned()),
        }
    }
}

fn recover_locked(directory: &File) -> io::Result<Recovery> {
    let mut recovery = Recovery {
        removed: 0,
        left: Vec::new(),
    };
    for entry in Dir::read_from(directory)? {
        let entry = entry?;
        let name = OsStr::from_bytes(entry.file_name().to_bytes());
        if !is_stage(name.as_bytes()) {
            continue;
        }
        match recover_stage(directory, name) {
            Ok(Recovered::Removed) => recovery.removed += 1,
            Ok(Recovered::Held) => recovery.left.push((name.to_owned(), Left::Held)),
            Ok(Recovered::Untouched) => {}
            Err(error) => {
                tracing::warn!(%error, stage = ?name, "an abandoned state-file stage was left in place");
                recovery.left.push((name.to_owned(), Left::Failed(error)));
            }
        }
    }
    Ok(recovery)
}

fn recover_stage(directory: &File, name: &OsStr) -> io::Result<Recovered> {
    let named = match fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(named) => named,
        Err(rustix::io::Errno::NOENT) => return Ok(Recovered::Untouched),
        Err(error) => return Err(error.into()),
    };
    if FileType::from_raw_mode(named.st_mode) != FileType::RegularFile
        || named.st_uid != rustix::process::getuid().as_raw()
    {
        return Ok(Recovered::Untouched);
    }
    let mut file = None;
    for access in [OFlags::RDONLY, OFlags::WRONLY] {
        match fs::openat(
            directory,
            name,
            access | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(opened) => {
                file = Some(File::from(opened));
                break;
            }
            Err(rustix::io::Errno::ACCESS | rustix::io::Errno::PERM) => {}
            Err(rustix::io::Errno::NOENT | rustix::io::Errno::LOOP) => break,
            Err(error) => return Err(error.into()),
        }
    }
    let Some(file) = file else {
        return Ok(Recovered::Untouched);
    };
    let metadata = fs::fstat(&file)?;
    if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile
        || metadata.st_uid != rustix::process::getuid().as_raw()
    {
        return Ok(Recovered::Untouched);
    }
    let Some(abandoned) = Flock::take(file)? else {
        return Ok(Recovered::Held);
    };
    if !same_inode(directory, name, &abandoned)? || !unlink_obsolete(directory, name)? {
        return Ok(Recovered::Untouched);
    }
    Ok(Recovered::Removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sync_that_fails_after_publication_leaves_the_new_contents_for_every_reader() {
        for publish in [Publish::Replace, Publish::Create] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("state");
            let stage = AtomicFile::prepare(&path).unwrap();
            stage.as_file().write_all(b"new").unwrap();
            let failing = fail_syncs_after_publication();
            let published = stage.finish(publish).unwrap();
            drop(failing);
            assert!(published.durable.is_err());
            assert_eq!(std::fs::read(&path).unwrap(), b"new");
            assert_eq!(recover(dir.path()).unwrap(), 0);
        }
    }
}
