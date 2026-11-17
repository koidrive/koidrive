// SPDX-License-Identifier: Apache-2.0

use std::fs::File;
use std::io;
use std::ops::Deref;

use rustix::fs::FlockOperation;
use rustix::io::Errno;

/// An exclusive or shared `flock` on an open file or folder, unlocked when it is dropped. Closing the descriptor is not
/// enough. A close releases the lock only once no copy of the descriptor is left, and a child process holds a copy
/// of every descriptor of its parent from its fork to its exec, close-on-exec or not.
#[derive(Debug)]
pub struct Flock(File);

impl Flock {
    /// Takes the lock on `file` without waiting. Returns `None` while another open of the file holds it, in this
    /// process or another.
    pub fn take(file: File) -> io::Result<Option<Self>> {
        Self::try_lock(file, FlockOperation::NonBlockingLockExclusive)
    }

    pub fn share(file: File) -> io::Result<Option<Self>> {
        Self::try_lock(file, FlockOperation::NonBlockingLockShared)
    }

    /// Takes the lock on `file`, waiting while another open of the file holds it.
    pub fn wait(file: File) -> io::Result<Self> {
        rustix::fs::flock(&file, FlockOperation::LockExclusive)?;
        Ok(Self(file))
    }

    fn try_lock(file: File, operation: FlockOperation) -> io::Result<Option<Self>> {
        match rustix::fs::flock(&file, operation) {
            Ok(()) => Ok(Some(Self(file))),
            Err(Errno::WOULDBLOCK) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
}

impl Deref for Flock {
    type Target = File;

    fn deref(&self) -> &File {
        &self.0
    }
}

impl Drop for Flock {
    fn drop(&mut self) {
        if let Err(error) = rustix::fs::flock(&self.0, FlockOperation::Unlock) {
            tracing::warn!(%error, "a lock was not unlocked; it is released when its last descriptor closes");
        }
    }
}

#[cfg(test)]
mod tests {
    use koidrive_test_support::Forked;

    use super::*;

    fn open(dir: &tempfile::TempDir) -> File {
        File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.path().join("lock"))
            .unwrap()
    }

    #[test]
    fn a_dropped_lock_is_free_while_a_child_process_starts() {
        let dir = tempfile::tempdir().unwrap();
        let locks: [fn(File) -> Flock; 2] = [
            |file| Flock::take(file).unwrap().unwrap(),
            |file| Flock::wait(file).unwrap(),
        ];
        for lock in locks {
            let lock = lock(open(&dir));
            let child = Forked::start();
            drop(lock);
            let next = Flock::take(open(&dir)).unwrap();
            drop(child);
            assert!(next.is_some());
        }
    }

    #[test]
    fn a_dropped_shared_lock_is_free_while_a_child_process_starts() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Flock::share(open(&dir)).unwrap().unwrap();
        let child = Forked::start();
        drop(shared);
        let next = Flock::take(open(&dir)).unwrap();
        drop(child);
        assert!(next.is_some());
    }

    #[test]
    fn a_held_lock_stops_another_open() {
        let dir = tempfile::tempdir().unwrap();
        let _held = Flock::take(open(&dir)).unwrap().unwrap();
        assert!(Flock::take(open(&dir)).unwrap().is_none());
        assert!(Flock::share(open(&dir)).unwrap().is_none());
    }
}
