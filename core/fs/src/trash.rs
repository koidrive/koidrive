// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsFd, BorrowedFd};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustix::fs::{AtFlags, OFlags};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};

use crate::resolve::{Folder, Space, check_name};
use crate::root::{Flush, rename_no_replace, replace_file};
use crate::{FileType, list, stat_at};

/// The folder inside the private folder that holds the local trash.
pub const TRASH_DIR: &str = "trash";
/// The longest an entry stays in the local trash.
pub const TRASH_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

const INDEX: &str = "trash.json";
const INDEX_TEMP: &str = "trash.json.tmp";

#[derive(Debug, Default, Serialize, Deserialize)]
struct Index {
    entries: BTreeMap<String, Kept>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Kept {
    name: String,
    folder: bool,
    size: u64,
    ctime_ns: i64,
    #[serde(default)]
    content: Option<Vec<u8>>,
    trashed_s: u64,
    recorded: bool,
}

/// What one reclamation did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reclaimed {
    /// Trash entries that were deleted.
    pub removed: Vec<String>,
    /// Names at the sync root of entries that changed after they entered the
    /// trash, or whose values were never recorded, and were recovered instead
    /// of deleted.
    pub recovered: Vec<OsString>,
}

impl Space {
    /// Moves `name` in `from` into the local trash as `entry`, never
    /// replacing anything there, and flushes both folders. Before it moves, the
    /// entry's size and ctime are recorded with `original`, the name it had
    /// before it was displaced, and with `content`, what the caller identifies
    /// a file's content by. All three names must be single file names. `from`
    /// and the trash are checked from the root again first.
    pub fn trash(
        &self,
        from: &Folder,
        name: &OsStr,
        entry: &str,
        original: &OsStr,
        content: Option<&[u8]>,
        now: SystemTime,
    ) -> io::Result<()> {
        self.trash_with(from, name, entry, original, content, now, &mut || {})
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn trash_with(
        &self,
        from: &Folder,
        name: &OsStr,
        entry: &str,
        original: &OsStr,
        content: Option<&[u8]>,
        now: SystemTime,
        cut: &mut dyn FnMut(),
    ) -> io::Result<()> {
        for name in [name, OsStr::new(entry), original] {
            check_name(name)?;
        }
        let trash = self.private(&[TRASH_DIR])?;
        self.recheck(&trash)?;
        if stat_at(trash.fd(), OsStr::new(entry)).is_ok() {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists));
        }
        self.recheck(from)?;
        let original = original.to_string_lossy();
        let kept = kept(from.fd(), name, &original, content, now, true)?;
        let private = self.private(&[])?;
        let mut index = read_index(private.fd())?;
        index.entries.insert(entry.to_owned(), kept);
        self.recheck(&private)?;
        write_index(private.fd(), &index, Flush::Plain)?;
        cut();
        self.recheck(from)?;
        self.recheck(&trash)?;
        rename_no_replace(from.fd(), name, trash.fd(), OsStr::new(entry))?;
        self.flush_folder(from, Flush::Plain)?;
        self.flush_folder(&trash, Flush::Barrier)
    }

    /// Reclaims the local trash at `now`: entries older than
    /// [`TRASH_RETENTION`], then the oldest entries until the recorded sizes
    /// of those left add up to at most `cap` bytes. A file whose size differs
    /// from the index, or that `holds` does not find holding the recorded
    /// content, or a folder that is no longer empty, is recovered at the sync
    /// root instead of deleted. An entry the index does not know is indexed
    /// now, counted as entering now, and recovered rather than deleted when it
    /// is due. An index entry with no entry, left by a crash before the move,
    /// is dropped. The trash folder is checked from the root again before each
    /// entry.
    pub fn reclaim_trash(
        &self,
        now: SystemTime,
        cap: u64,
        holds: &mut dyn FnMut(&File, &[u8]) -> io::Result<bool>,
    ) -> io::Result<Reclaimed> {
        let trash = self.private(&[TRASH_DIR])?;
        let private = self.private(&[])?;
        let root = self.root_folder()?;
        let mut index = read_index(private.fd())?;
        let names = list(trash.fd())?;
        index
            .entries
            .retain(|entry, _| names.contains(OsStr::new(entry)));
        for name in &names {
            if let Some(entry) = name.to_str()
                && !index.entries.contains_key(entry)
            {
                let kept = kept(trash.fd(), name, entry, None, now, false)?;
                index.entries.insert(entry.to_owned(), kept);
            }
        }

        let mut order: Vec<(String, Kept)> = index
            .entries
            .iter()
            .map(|(entry, kept)| (entry.clone(), kept.clone()))
            .collect();
        order.sort_by(|(a, x), (b, y)| (x.trashed_s, a).cmp(&(y.trashed_s, b)));
        let now_s = seconds(now);
        let mut total: u64 = order.iter().map(|(_, kept)| kept.size).sum();
        let mut reclaimed = Reclaimed::default();
        for (entry, kept) in order {
            let expired = kept.trashed_s.saturating_add(TRASH_RETENTION.as_secs()) <= now_s;
            if !expired && total <= cap {
                break;
            }
            total -= kept.size;
            self.recheck(&trash)?;
            if kept.recorded && remove_unchanged(trash.fd(), &entry, &kept, holds)? {
                reclaimed.removed.push(entry.clone());
            } else {
                let original = match check_name(OsStr::new(&kept.name)) {
                    Ok(()) => &kept.name,
                    Err(_) => &entry,
                };
                let path = self.move_to_recovered_name(
                    &trash,
                    OsStr::new(&entry),
                    &root,
                    OsStr::new(original),
                    kept.folder,
                    now,
                )?;
                reclaimed.recovered.push(path.into_os_string());
            }
            index.entries.remove(&entry);
        }
        self.recheck(&private)?;
        write_index(private.fd(), &index, Flush::Full)?;
        Ok(reclaimed)
    }
}

fn remove_unchanged(
    trash: BorrowedFd<'_>,
    entry: &str,
    kept: &Kept,
    holds: &mut dyn FnMut(&File, &[u8]) -> io::Result<bool>,
) -> io::Result<bool> {
    let stat = stat_at(trash, OsStr::new(entry))?;
    let folder = stat.file_type == FileType::Directory;
    if folder != kept.folder || !folder && stat.size != kept.size {
        return Ok(false);
    }
    if !folder {
        let unchanged = match &kept.content {
            Some(content) => {
                let flags = OFlags::RDONLY | OFlags::NOFOLLOW;
                koidrive_state_file::open_regular(trash, entry, flags)
                    .is_ok_and(|file| holds(&file, content).unwrap_or(false))
            }
            None => ctime(stat.ctime_ns) == kept.ctime_ns,
        };
        if !unchanged {
            return Ok(false);
        }
    }
    let flags = if folder {
        AtFlags::REMOVEDIR
    } else {
        AtFlags::empty()
    };
    match rustix::fs::unlinkat(trash, entry, flags) {
        Ok(()) => Ok(true),
        Err(Errno::NOTEMPTY | Errno::EXIST) if folder => Ok(false),
        Err(err) => Err(err.into()),
    }
}

fn kept(
    dir: BorrowedFd<'_>,
    entry: &OsStr,
    name: &str,
    content: Option<&[u8]>,
    now: SystemTime,
    recorded: bool,
) -> io::Result<Kept> {
    let stat = stat_at(dir, entry)?;
    let folder = stat.file_type == FileType::Directory;
    Ok(Kept {
        name: name.to_owned(),
        folder,
        size: if folder { 0 } else { stat.size },
        ctime_ns: ctime(stat.ctime_ns),
        content: content.filter(|_| !folder).map(<[u8]>::to_vec),
        trashed_s: seconds(now),
        recorded,
    })
}

fn ctime(ns: i128) -> i64 {
    i64::try_from(ns).unwrap_or(i64::MAX)
}

fn seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

fn read_index(private: BorrowedFd<'_>) -> io::Result<Index> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW;
    let mut file = match koidrive_state_file::open_regular(private, INDEX, flags) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Index::default()),
        Err(err) => return Err(err),
    };
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    serde_json::from_str(&text).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

fn write_index(private: BorrowedFd<'_>, index: &Index, then: Flush) -> io::Result<()> {
    replace_file(
        private.as_fd(),
        INDEX,
        INDEX_TEMP,
        &serde_json::to_vec_pretty(index)?,
        then,
    )
}
