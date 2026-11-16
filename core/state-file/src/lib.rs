// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

use rustix::fs::{self, AtFlags, Dir, FileType, FlockOperation, Mode, OFlags, flock};

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
    file: File,
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
        let directory = open_directory(parent(path))?;
        let lock = DirectoryLock::new(&directory)?;
        recover_locked(&directory)?;
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
            if let Err(error) = flock(&file, FlockOperation::LockExclusive) {
                let _ = fs::unlinkat(&directory, &stage, AtFlags::empty());
                return Err(error.into());
            }
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
    pub fn finish(mut self, publish: Publish) -> io::Result<File> {
        self.file.sync_all()?;
        let _lock = DirectoryLock::new(&self.directory)?;
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
        self.directory.sync_all()?;
        flock(&self.file, FlockOperation::Unlock)?;
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
    let stage = AtomicFile::prepare(path)?;
    stage.as_file().write_all(bytes)?;
    stage.finish(Publish::Replace).map(drop)
}

/// Removes this helper's abandoned stages, but never a stage locked by a live writer.
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

fn recover_locked(directory: &File) -> io::Result<usize> {
    use std::os::unix::ffi::OsStrExt;
    let mut recovered = 0;
    for entry in Dir::read_from(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(random) = name
            .to_bytes()
            .strip_prefix(PREFIX.as_bytes())
            .and_then(|name| name.strip_suffix(SUFFIX.as_bytes()))
        else {
            continue;
        };
        if random.len() != RANDOM_BYTES * 2 || !random.iter().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        let file = match fs::openat(
            directory,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(file) => File::from(file),
            Err(rustix::io::Errno::NOENT | rustix::io::Errno::LOOP) => continue,
            Err(error) => return Err(error.into()),
        };
        let metadata = fs::fstat(&file)?;
        if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile
            || metadata.st_uid != rustix::process::getuid().as_raw()
        {
            continue;
        }
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {
                if same_inode(
                    directory,
                    std::ffi::OsStr::from_bytes(name.to_bytes()),
                    &file,
                )? {
                    fs::unlinkat(directory, name, AtFlags::empty())?;
                    recovered += 1;
                }
            }
            Err(rustix::io::Errno::WOULDBLOCK) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(recovered)
}
