// SPDX-License-Identifier: Apache-2.0

use std::ffi::{OsStr, OsString};
use std::fs::{DirBuilder, File};
use std::io::{self, Write};
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
        recover_locked(&directory)?;
        if aliases_stage(&directory, &destination)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
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
    /// `Create` never replaces an existing destination. Failure after publication can leave the complete file.
    pub fn finish(self, publish: Publish) -> io::Result<File> {
        self.finish_with_legacy(publish, &[])
    }

    /// Selects obsolete regular files and symlinks from the anchored directory, then publishes and removes them.
    /// Callers must own the selected names and exclude concurrent writers of those legacy stages.
    /// Cleanup follows the error contract of [`Self::finish_with_legacy`].
    pub fn finish_with_legacy_matching(
        self,
        publish: Publish,
        legacy: impl FnMut(&OsStr) -> bool,
    ) -> io::Result<File> {
        let names = matching_legacy(&self.directory, legacy)?;
        self.finish_with_legacy(
            publish,
            &names.iter().map(OsString::as_os_str).collect::<Vec<_>>(),
        )
    }

    /// Publishes the file, then removes obsolete sibling stages while the directory is locked.
    /// Callers must own the legacy names. Names cannot escape the directory, name the destination,
    /// or overlap the helper's staging namespace. Permission-denied obsolete stages are left untouched.
    /// Other cleanup failures can follow complete publication.
    pub fn finish_with_legacy(mut self, publish: Publish, legacy: &[&OsStr]) -> io::Result<File> {
        validate_legacy(&self.destination, legacy)?;
        self.file.sync_all()?;
        let _lock = DirectoryLock::new(&self.directory)?;
        if aliases_stage(&self.directory, &self.destination)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a state-file destination cannot alias a helper stage",
            ));
        }
        for name in legacy {
            if aliases_stage(&self.directory, name)? {
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
            Publish::Create => {
                fs::linkat(
                    &self.directory,
                    stage,
                    &self.directory,
                    &self.destination,
                    AtFlags::empty(),
                )?;
                fs::unlinkat(&self.directory, stage, AtFlags::empty())?;
            }
        }
        self.stage = None;
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
        self.directory.sync_all()?;
        self.file.try_clone()
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

/// Replaces a state file with complete, synced bytes using [`AtomicFile`].
pub fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_with_legacy(path, bytes, &[])
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
            if let Err(error) = recorded {
                let _ = std::fs::remove_dir(root);
                return Err(error);
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
        && match (std::fs::metadata(root), std::fs::read_to_string(&record)) {
            (Ok(folder), Ok(text)) => text == made_record(&folder),
            _ => false,
        }
}

fn made_record(folder: &std::fs::Metadata) -> String {
    format!("{} {}\n", folder.dev(), folder.ino())
}

/// Replaces a state file and removes obsolete sibling stages owned by the caller.
/// Permission-denied obsolete stages are left untouched; other cleanup failures can follow publication.
pub fn write_with_legacy(path: &Path, bytes: &[u8], legacy: &[&OsStr]) -> io::Result<()> {
    let stage = AtomicFile::prepare(path)?;
    stage.as_file().write_all(bytes)?;
    stage.finish_with_legacy(Publish::Replace, legacy).map(drop)
}

/// Removes a state file and obsolete sibling stages, then syncs the directory without allocating a stage.
/// Selects obsolete regular files and symlinks from the anchored directory. Callers must own the selected
/// names and exclude concurrent writers of the destination and legacy stages.
/// Missing files or directories are already removed. Destination removal errors are returned.
/// Permission-denied obsolete stages are left untouched; other cleanup failures can follow destination removal.
pub fn remove_with_legacy_matching(
    path: &Path,
    legacy: impl FnMut(&OsStr) -> bool,
) -> io::Result<()> {
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
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let _lock = DirectoryLock::new(&directory)?;
    recover_locked(&directory)?;
    let names = matching_legacy(&directory, legacy)?;
    let legacy: Vec<_> = names.iter().map(OsString::as_os_str).collect();
    validate_legacy(destination, &legacy)?;
    for name in std::iter::once(&destination).chain(&legacy) {
        if aliases_stage(&directory, name)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a removed name cannot alias a helper stage",
            ));
        }
    }
    match fs::unlinkat(&directory, destination, AtFlags::empty()) {
        Ok(()) | Err(rustix::io::Errno::NOENT) => {}
        Err(error) => return Err(error.into()),
    }
    for name in legacy {
        unlink_obsolete(&directory, name)?;
    }
    directory.sync_all()
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
    let recovered = recover_locked(&directory)?;
    directory.sync_all()?;
    Ok(recovered)
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

fn aliases_stage(directory: &File, name: &OsStr) -> io::Result<bool> {
    let named = match fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(named) => named,
        Err(rustix::io::Errno::NOENT) => return Ok(false),
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
            return Ok(true);
        }
    }
    Ok(false)
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

fn recover_locked(directory: &File) -> io::Result<usize> {
    let mut recovered = 0;
    for entry in Dir::read_from(directory)? {
        let entry = entry?;
        let name = OsStr::from_bytes(entry.file_name().to_bytes());
        if !is_stage(name.as_bytes()) {
            continue;
        }
        let named = match fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(named) => named,
            Err(rustix::io::Errno::NOENT) => continue,
            Err(error) => return Err(error.into()),
        };
        if FileType::from_raw_mode(named.st_mode) != FileType::RegularFile
            || named.st_uid != rustix::process::getuid().as_raw()
        {
            continue;
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
            continue;
        };
        let metadata = fs::fstat(&file)?;
        if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile
            || metadata.st_uid != rustix::process::getuid().as_raw()
        {
            continue;
        }
        if let Some(abandoned) = Flock::take(file)?
            && same_inode(directory, name, &abandoned)?
        {
            recovered += usize::from(unlink_obsolete(directory, name)?);
        }
    }
    Ok(recovered)
}
