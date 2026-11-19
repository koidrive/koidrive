use crate::app_install::error::{AppInstallError, AppInstallFailure, failure};
use crate::app_install::macos::control::{Control, io_error, step};
use crate::app_install::macos::state::{Identity, OwnedPath, PinnedDirectory};
use rustix::fs::{AtFlags, FlockOperation, Mode, OFlags};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

impl PinnedDirectory {
    pub(super) fn new(file: File) -> Result<Self, AppInstallError> {
        let identity = identity(&file.metadata().map_err(io_error)?);
        Ok(Self { file, identity })
    }
}
pub(super) struct InstallerLock(pub(super) File);
impl Drop for InstallerLock {
    fn drop(&mut self) {
        let _ = rustix::fs::flock(&self.0, FlockOperation::Unlock);
    }
}
impl OwnedPath {
    pub(super) fn capture(path: PathBuf) -> Result<Self, AppInstallError> {
        let metadata = fs::symlink_metadata(&path).map_err(io_error)?;
        if metadata.file_type().is_symlink() {
            return Err(failure(
                AppInstallFailure::UnsafePath,
                "Symlinked transaction object",
            ));
        }
        Ok(Self {
            path,
            identity: identity(&metadata),
        })
    }

    pub(super) fn directory(&self) -> Result<File, AppInstallError> {
        let file = directory(&self.path, false)?;
        if identity(&file.metadata().map_err(io_error)?) != self.identity {
            return Err(failure(
                AppInstallFailure::ConcurrentChange,
                "Transaction directory changed",
            ));
        }
        Ok(file)
    }
}
pub(super) fn directory(path: &Path, create: bool) -> Result<File, AppInstallError> {
    if create {
        crate::lifeline::confine(path)
            .map_err(|error| failure(AppInstallFailure::UnsafePath, error))?;
    }
    let mut fd = rustix::fs::open(
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(io_error)?;
    for component in path.components() {
        match component {
            Component::RootDir => continue,
            Component::Normal(name) => {
                if create {
                    match rustix::fs::mkdirat(&fd, name, Mode::from_raw_mode(0o700)) {
                        Ok(()) | Err(rustix::io::Errno::EXIST) => (),
                        Err(error) => return Err(io_error(error)),
                    }
                }
                fd = rustix::fs::openat(
                    &fd,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|error| {
                    failure(
                        AppInstallFailure::UnsafePath,
                        format!("Unsafe directory {}: {error}", path.display()),
                    )
                })?;
            }
            _ => {
                return Err(failure(
                    AppInstallFailure::UnsafePath,
                    "Installer paths must be absolute and normalized",
                ));
            }
        }
    }
    if !path.is_absolute() {
        return Err(failure(
            AppInstallFailure::UnsafePath,
            "Installer directory is not absolute",
        ));
    }
    Ok(fd.into())
}
pub(super) fn identity(metadata: &fs::Metadata) -> Identity {
    Identity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}
pub(super) fn remove_owned_contents(
    directory: &File,
    device: u64,
    control: &Control,
    remaining: &mut usize,
    depth: usize,
) -> Result<(), AppInstallError> {
    if depth > 128 {
        return Err(failure(
            AppInstallFailure::UnsafePath,
            "Retired app directory nesting exceeds limit",
        ));
    }
    for entry in rustix::fs::Dir::read_from(directory).map_err(io_error)? {
        control.check()?;
        let entry = entry.map_err(io_error)?;
        let name = entry.file_name();
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        *remaining = remaining.checked_sub(1).ok_or_else(|| {
            failure(
                AppInstallFailure::UnsafePath,
                "Retired app entry count exceeds limit",
            )
        })?;
        let stat =
            rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW).map_err(io_error)?;
        if stat.st_dev as u64 != device {
            return Err(failure(
                AppInstallFailure::UnsafePath,
                "Retired app contains a mounted filesystem",
            ));
        }
        let flags = match rustix::fs::FileType::from_raw_mode(stat.st_mode) {
            rustix::fs::FileType::Directory => {
                let child = File::from(
                    rustix::fs::openat(
                        directory,
                        name,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(io_error)?,
                );
                let metadata = child.metadata().map_err(io_error)?;
                if metadata.dev() != device || metadata.ino() != stat.st_ino {
                    return Err(failure(
                        AppInstallFailure::ConcurrentChange,
                        "Retired app child changed",
                    ));
                }
                remove_owned_contents(&child, device, control, remaining, depth + 1)?;
                AtFlags::REMOVEDIR
            }
            rustix::fs::FileType::RegularFile => AtFlags::empty(),
            rustix::fs::FileType::Symlink => {
                let target =
                    rustix::fs::readlinkat(directory, name, Vec::new()).map_err(io_error)?;
                let mut level = depth;
                for component in Path::new(OsStr::from_bytes(target.as_bytes())).components() {
                    match component {
                        Component::CurDir => (),
                        Component::Normal(_) => level += 1,
                        Component::ParentDir if level > 0 => level -= 1,
                        _ => {
                            return Err(failure(
                                AppInstallFailure::UnsafePath,
                                "Retired app contains an escaping symlink",
                            ));
                        }
                    }
                }
                AtFlags::empty()
            }
            _ => {
                return Err(failure(
                    AppInstallFailure::UnsafePath,
                    "Retired app contains a special file",
                ));
            }
        };
        let current =
            rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW).map_err(io_error)?;
        if current.st_dev != stat.st_dev || current.st_ino != stat.st_ino {
            return Err(failure(
                AppInstallFailure::ConcurrentChange,
                "Retired app entry changed before removal",
            ));
        }
        rustix::fs::unlinkat(directory, name, flags).map_err(io_error)?;
    }
    Ok(())
}
pub(super) fn unchanged_directory(path: &Path, fd: &File) -> Result<(), AppInstallError> {
    let current = directory(path, false)?;
    if identity(&current.metadata().map_err(io_error)?)
        != identity(&fd.metadata().map_err(io_error)?)
    {
        return Err(failure(
            AppInstallFailure::ConcurrentChange,
            "Applications directory changed during installation",
        ));
    }
    Ok(())
}
pub(super) async fn lock(
    root: &File,
    within: Duration,
    control: &Control,
) -> Result<InstallerLock, AppInstallError> {
    let name = ".koidrive-app-install.lock";
    let file = match rustix::fs::openat(
        root,
        name,
        OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o644),
    ) {
        Ok(fd) => File::from(fd),
        Err(rustix::io::Errno::EXIST) => {
            koidrive_state_file::open_regular(root, name, OFlags::RDONLY).map_err(io_error)?
        }
        Err(error) => return Err(io_error(error)),
    };
    let metadata = file.metadata().map_err(io_error)?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(failure(
            AppInstallFailure::UnsafePath,
            "Unsafe installer lock",
        ));
    }
    let acquire = async {
        loop {
            match rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => return Ok(()),
                Err(rustix::io::Errno::WOULDBLOCK) => {
                    tokio::time::sleep(Duration::from_millis(40)).await
                }
                Err(error) => return Err(io_error(error)),
            }
        }
    };
    let waiting = "Waiting for another Koidrive install to finish";
    step(waiting, control.bounded(waiting, within, acquire))
        .await
        .map_err(|error| match error.kind {
            AppInstallFailure::Stalled => failure(
                AppInstallFailure::InUse,
                format!(
                    "Another Koidrive install held the install lock for {} seconds",
                    within.as_secs()
                ),
            ),
            _ => error,
        })?;
    let guard = InstallerLock(file);
    let current = rustix::fs::statat(root, name, AtFlags::SYMLINK_NOFOLLOW).map_err(io_error)?;
    if current.st_ino != metadata.ino() || current.st_dev as u64 != metadata.dev() {
        return Err(failure(
            AppInstallFailure::ConcurrentChange,
            "Installer lock changed",
        ));
    }
    Ok(guard)
}
pub(crate) fn writable(root: &Path) -> Result<bool, AppInstallError> {
    crate::lifeline::confine(&root.join(".koidrive-write"))
        .map_err(|error| failure(AppInstallFailure::UnsafePath, error))?;
    match tempfile::Builder::new()
        .prefix(".koidrive-write-")
        .tempdir_in(root)
    {
        Ok(probe) => {
            probe.close().map_err(io_error)?;
            Ok(true)
        }
        Err(error)
            if error.kind() == std::io::ErrorKind::PermissionDenied
                || matches!(error.raw_os_error(), Some(13 | 30 | 1)) =>
        {
            Ok(false)
        }
        Err(error) => Err(io_error(error)),
    }
}
pub(super) fn private_work(path: &Path, create: bool) -> Result<File, AppInstallError> {
    let file = directory(path, create)?;
    let metadata = file.metadata().map_err(io_error)?;
    if metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o077 != 0 {
        return Err(failure(
            AppInstallFailure::UnsafePath,
            "Installer work root is not private",
        ));
    }
    Ok(file)
}
pub(super) fn regular_at(root: &File, name: &OsStr) -> Result<File, AppInstallError> {
    let file = koidrive_state_file::open_regular(root, name, OFlags::RDONLY | OFlags::NOFOLLOW)
        .map_err(|error| failure(AppInstallFailure::UnsafePath, error))?;
    let metadata = file.metadata().map_err(io_error)?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != rustix::process::geteuid().as_raw()
    {
        return Err(failure(
            AppInstallFailure::UnsafePath,
            "Unsafe transaction file",
        ));
    }
    Ok(file)
}
pub(super) fn observed(path: &Path) -> Result<Option<Identity>, AppInstallError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            Ok(Some(identity(&metadata)))
        }
        Ok(_) => Err(failure(
            AppInstallFailure::UnsafePath,
            "Unexpected transaction object",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error(error)),
    }
}
pub(super) fn only_entries(root: &File, allowed: &[&OsStr]) -> Result<(), AppInstallError> {
    for entry in rustix::fs::Dir::read_from(root).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let name = entry.file_name();
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        if !allowed
            .iter()
            .any(|allowed| allowed.as_encoded_bytes() == name.to_bytes())
        {
            return Err(failure(
                AppInstallFailure::UnsafePath,
                "Transaction contains an unowned entry",
            ));
        }
    }
    Ok(())
}
