// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rustix::fs::{Mode, OFlags, XattrFlags};
use rustix::io::Errno;

use crate::resolve::{Location, Space, Unresolved};
use crate::{Entry, FileId, stat_fd};

#[cfg(target_vendor = "apple")]
const NAME: &str = "com.koidrive.node";
#[cfg(not(target_vendor = "apple"))]
const NAME: &str = "user.koidrive.node";

impl Space {
    /// Marks the entry at `at` with `node` when it is still the entry `expected`, so that a recovery
    /// finds the node of an entry the store lost track of (design section 6.4). An entry on a volume
    /// that keeps extended attributes in companion files, or one whose attribute cannot be written,
    /// gets no mark.
    pub fn mark(&self, at: &Location, expected: FileId, node: &str) -> io::Result<()> {
        if let Some(entry) = self.open_entry(at, expected)?
            && let Some((before, after)) = write(entry.as_fd(), node)
        {
            self.marks.record(expected, before, after);
        }
        Ok(())
    }

    /// The change times this space's marks moved, for the scans of its root.
    pub fn marks(&self) -> MarkLog {
        self.marks.clone()
    }

    /// The node the mark on the entry at `at` names, when the entry is still `expected` and the
    /// mark was written on this entry rather than copied with it.
    pub fn mark_of(&self, at: &Location, expected: FileId) -> io::Result<Option<String>> {
        match self.open_entry(at, expected)? {
            Some(entry) => node_mark(entry.as_fd()),
            None => Ok(None),
        }
    }

    fn open_entry(&self, at: &Location, expected: FileId) -> io::Result<Option<OwnedFd>> {
        let (parent, name) = match self.parent(at) {
            Ok(found) => found,
            Err(
                Unresolved::Missing(_) | Unresolved::NotAFolder(_) | Unresolved::Replaced { .. },
            ) => {
                return Ok(None);
            }
            Err(err) => return Err(err.into()),
        };
        let Some(entry) = open(parent.fd(), name)? else {
            return Ok(None);
        };
        Ok((stat_fd(entry.as_fd())?.id.same(&expected)).then_some(entry))
    }
}

/// The change times marks moved: for each marked entry, its change time before and after its latest mark.
#[derive(Debug, Clone, Default)]
pub struct MarkLog(Arc<Mutex<HashMap<FileId, (i128, i128)>>>);

impl MarkLog {
    fn record(&self, id: FileId, before: i128, after: i128) {
        self.0.lock().unwrap().insert(id, (before, after));
    }

    pub(crate) fn carry(&self, old: &mut BTreeMap<PathBuf, Entry>, new: &BTreeMap<PathBuf, Entry>) {
        let mut marks = self.0.lock().unwrap();
        if marks.is_empty() {
            return;
        }
        for (path, before) in old.iter_mut() {
            if let Some(now) = new.get(path)
                && now.id == before.id
                && marks.get(&now.id) == Some(&(before.ctime.at(), now.ctime.at()))
            {
                before.ctime = now.ctime;
            }
        }
        let seen: HashSet<FileId> = new.values().map(|now| now.id).collect();
        let gone: HashSet<FileId> = (old.values())
            .map(|before| before.id)
            .filter(|id| !seen.contains(id))
            .collect();
        let raced: HashSet<FileId> = (new.values())
            .filter(|now| {
                marks
                    .get(&now.id)
                    .is_some_and(|&(was, _)| was == now.ctime.at())
            })
            .map(|now| now.id)
            .collect();
        marks.retain(|id, _| raced.contains(id) || !(seen.contains(id) || gone.contains(id)));
    }
}

pub(crate) fn open(folder: BorrowedFd<'_>, name: &OsStr) -> io::Result<Option<OwnedFd>> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    match rustix::fs::openat(folder, name, flags, Mode::empty()) {
        Ok(entry) => Ok(Some(entry)),
        Err(Errno::NOENT | Errno::NOTDIR | Errno::LOOP) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

pub(crate) fn write(entry: BorrowedFd<'_>, node: &str) -> Option<(i128, i128)> {
    let stat = stat_fd(entry).ok()?;
    if !native(entry) {
        return None;
    }
    let value = encode(node, stat.id);
    if rustix::fs::fsetxattr(entry, NAME, value.as_bytes(), XattrFlags::empty()).is_err() {
        let _ = rustix::fs::fremovexattr(entry, NAME);
        return None;
    }
    Some((stat.ctime_ns, stat_fd(entry).ok()?.ctime_ns))
}

/// The node the mark on the open entry `entry` names, when the mark was written on this entry rather than copied
/// with it.
pub fn node_mark(entry: BorrowedFd<'_>) -> io::Result<Option<String>> {
    let mut value = [0; 256];
    let len = match rustix::fs::fgetxattr(entry, NAME, &mut value) {
        Ok(len) => len,
        Err(Errno::NOTSUP | Errno::RANGE) => return Ok(None),
        #[cfg(target_vendor = "apple")]
        Err(Errno::NOATTR) => return Ok(None),
        #[cfg(not(target_vendor = "apple"))]
        Err(Errno::NODATA) => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let id = stat_fd(entry)?.id;
    Ok(decode(&value[..len])
        .filter(|(_, marked)| *marked == id.without_handle())
        .map(|(node, _)| node))
}

fn encode(node: &str, id: FileId) -> String {
    let birth = id
        .birth_ns
        .map_or_else(|| "-".to_owned(), |birth| birth.to_string());
    format!("2 {node} {} {birth}", id.ino)
}

fn decode(value: &[u8]) -> Option<(String, FileId)> {
    let value = std::str::from_utf8(value).ok()?;
    let fields: Vec<&str> = value.split(' ').collect();
    let (node, ino, birth) = match fields[..] {
        ["2", node, ino, birth] | ["1", node, _, ino, birth] => (node, ino, birth),
        _ => return None,
    };
    if node.is_empty() {
        return None;
    }
    let birth_ns = match birth {
        "-" => None,
        birth => Some(birth.parse().ok()?),
    };
    let id = FileId {
        ino: ino.parse().ok()?,
        birth_ns,
        handle: None,
    };
    Some((node.to_owned(), id))
}

#[cfg(target_vendor = "apple")]
fn native(entry: BorrowedFd<'_>) -> bool {
    let Ok(volume) = rustix::fs::fstatfs(entry) else {
        return false;
    };
    let name: Vec<u8> = (volume.f_fstypename.iter())
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect();
    matches!(name.as_slice(), b"apfs" | b"hfs")
}

#[cfg(not(target_vendor = "apple"))]
fn native(_: BorrowedFd<'_>) -> bool {
    true
}
