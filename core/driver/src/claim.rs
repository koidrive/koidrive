// SPDX-License-Identifier: Apache-2.0

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use koidrive_fs::PRIVATE_DIR;
use koidrive_state_file::Flock;
use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};

const OWNER: &str = "device.json";
const OWNER_TEMP: &str = "device.json.tmp";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Owner {
    pub server: String,
    pub device_id: String,
}

fn private_dir(root: &Path) -> io::Result<OwnedFd> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let root = rustix::fs::open(root, flags, Mode::empty())?;
    Ok(rustix::fs::openat(
        &root,
        PRIVATE_DIR,
        flags | OFlags::NOFOLLOW,
        Mode::empty(),
    )?)
}

pub(crate) fn lock(root: &Path) -> io::Result<Option<Flock>> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    Flock::take(File::from(rustix::fs::open(root, flags, Mode::empty())?))
}

/// What tells one folder from another on the volume the root check vouches for, whatever path reaches it: its inode
/// and, where the file system reports it, its birth time, which tells a reused inode number apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    ino: u64,
    born: Option<SystemTime>,
}

impl Identity {
    pub fn of(meta: &fs::Metadata) -> Self {
        Identity {
            ino: meta.ino(),
            born: meta.created().ok(),
        }
    }
}

pub(crate) fn still_locked(lock: &File, root: &Path) -> bool {
    match (lock.metadata(), fs::metadata(root)) {
        (Ok(held), Ok(now)) => held.dev() == now.dev() && held.ino() == now.ino(),
        _ => false,
    }
}

pub(crate) fn owner(root: &Path) -> io::Result<Option<Owner>> {
    let dir = private_dir(root)?;
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let file = match rustix::fs::openat(&dir, OWNER, flags, Mode::empty()) {
        Ok(file) => file,
        Err(Errno::NOENT) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut text = String::new();
    File::from(file).read_to_string(&mut text)?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub(crate) fn set_owner(root: &Path, owner: &Owner) -> io::Result<()> {
    let dir = private_dir(root)?;
    match rustix::fs::unlinkat(&dir, OWNER_TEMP, rustix::fs::AtFlags::empty()) {
        Ok(()) | Err(Errno::NOENT) => {}
        Err(error) => return Err(error.into()),
    }
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut file = File::from(rustix::fs::openat(
        &dir,
        OWNER_TEMP,
        flags,
        Mode::from_raw_mode(0o644),
    )?);
    file.write_all(&serde_json::to_vec_pretty(owner)?)?;
    file.sync_all()?;
    rustix::fs::renameat(&dir, OWNER_TEMP, &dir, OWNER)?;
    rustix::fs::fsync(dir.as_fd())?;
    Ok(())
}

pub(crate) fn record(root: &Path) -> PathBuf {
    root.join(PRIVATE_DIR).join(OWNER)
}

pub(crate) fn check_release(root: &Path, owner: &Owner) -> io::Result<()> {
    if self::owner(root)?.as_ref() != Some(owner) {
        return Ok(());
    }
    let dir = private_dir(root)?;
    let access = rustix::fs::Access::WRITE_OK | rustix::fs::Access::EXEC_OK;
    rustix::fs::accessat(&dir, ".", access, rustix::fs::AtFlags::empty())?;
    Ok(())
}

pub(crate) fn release(root: &Path, owner: &Owner) -> io::Result<()> {
    let current = match self::owner(root) {
        Ok(current) => current,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return crate::leave::sync_parent(&record(root));
        }
        Err(error) => return Err(error),
    };
    if current.as_ref().is_some_and(|current| current != owner) {
        return Ok(());
    }
    let dir = private_dir(root)?;
    if current.is_some() {
        match rustix::fs::unlinkat(&dir, OWNER, rustix::fs::AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => {}
            Err(error) => return Err(error.into()),
        }
    }
    crate::leave::sync_dir(&File::from(dir))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fs;
    use std::process::{Child, Command};

    use rustix::io::FdFlags;

    use super::*;

    /// A child process that holds a copy of a descriptor until this is dropped. Every child holds such a copy from
    /// its fork to its exec. This one keeps it, so a test does not depend on that window.
    pub(crate) struct Holder(Child);

    impl Holder {
        pub(crate) fn of(file: &File) -> Self {
            let copy = OwnedFd::from(file.try_clone().unwrap());
            rustix::io::fcntl_setfd(&copy, FdFlags::empty()).unwrap();
            Self(Command::new("sleep").arg("600").spawn().unwrap())
        }
    }

    impl Drop for Holder {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn folder() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(PRIVATE_DIR)).unwrap();
        dir
    }

    fn device(id: &str) -> Owner {
        Owner {
            server: "https://files.example.com/".into(),
            device_id: id.into(),
        }
    }

    #[test]
    fn one_holder_at_a_time_takes_the_lock_whatever_path_reaches_the_folder() {
        let dir = folder();
        let other_path = tempfile::tempdir().unwrap();
        let alias = other_path.path().join("alias");
        std::os::unix::fs::symlink(dir.path(), &alias).unwrap();

        let held = lock(dir.path()).unwrap().unwrap();
        assert!(lock(&alias).unwrap().is_none());
        assert!(lock(dir.path()).unwrap().is_none());
        drop(held);
        assert!(lock(&alias).unwrap().is_some());
    }

    #[test]
    fn the_lock_is_released_while_a_child_process_holds_a_copy_of_its_descriptor() {
        let dir = folder();
        let held = lock(dir.path()).unwrap().unwrap();
        let _child = Holder::of(&held);
        assert!(lock(dir.path()).unwrap().is_none());
        drop(held);
        assert!(lock(dir.path()).unwrap().is_some());
    }

    #[test]
    fn shared_locks_are_released_while_a_child_process_holds_a_copy_of_a_descriptor() {
        let dir = folder();
        let open = || File::open(dir.path()).unwrap();
        let first = Flock::share(open()).unwrap().unwrap();
        let second = Flock::share(open()).unwrap().unwrap();
        let _child = Holder::of(&first);
        assert!(lock(dir.path()).unwrap().is_none());
        drop(first);
        drop(second);
        assert!(lock(dir.path()).unwrap().is_some());
    }

    #[test]
    fn the_lock_survives_the_loss_of_the_private_directory() {
        let dir = folder();
        let held = lock(dir.path()).unwrap().unwrap();
        fs::remove_dir_all(dir.path().join(PRIVATE_DIR)).unwrap();
        fs::create_dir(dir.path().join(PRIVATE_DIR)).unwrap();
        assert!(lock(dir.path()).unwrap().is_none());
        assert!(still_locked(&held, dir.path()));
    }

    #[test]
    fn a_moved_or_replaced_folder_is_no_longer_the_locked_one() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("root");
        fs::create_dir(&root).unwrap();
        let held = lock(&root).unwrap().unwrap();
        fs::rename(&root, parent.path().join("moved")).unwrap();
        assert!(!still_locked(&held, &root));
        fs::create_dir(&root).unwrap();
        assert!(!still_locked(&held, &root));
        assert!(lock(&root).unwrap().is_some());
    }

    #[test]
    fn the_owner_record_is_replaced_and_released_only_by_its_device() {
        let dir = folder();
        assert_eq!(owner(dir.path()).unwrap(), None);
        set_owner(dir.path(), &device("d1")).unwrap();
        assert_eq!(owner(dir.path()).unwrap(), Some(device("d1")));
        release(dir.path(), &device("d2")).unwrap();
        assert_eq!(owner(dir.path()).unwrap(), Some(device("d1")));
        release(dir.path(), &device("d1")).unwrap();
        assert_eq!(owner(dir.path()).unwrap(), None);
    }

    #[test]
    fn a_symbolic_link_in_place_of_the_private_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), dir.path().join(PRIVATE_DIR)).unwrap();
        assert!(set_owner(dir.path(), &device("d1")).is_err());
        assert!(fs::read_dir(elsewhere.path()).unwrap().next().is_none());
    }
}
