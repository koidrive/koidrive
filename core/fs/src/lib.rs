// SPDX-License-Identifier: Apache-2.0

//! File system change detection for a synced folder.
//!
//! A [`Snapshot`] records every file and folder under a root with its identity
//! (device, inode, birth time), size, modification time and executable bit.
//! [`Snapshot::rescan`] compares parts of the disk against it and reports typed
//! [`Change`]s, pairing a disappeared path with an appeared one of the same
//! identity as a rename. Watchers only decide which parts to rescan:
//!
//! - [`NativeWatcher`] listens to FSEvents on macOS and inotify on Linux and
//!   rescans the paths they name.
//! - [`RescanWatcher`] rescans the whole tree on an interval, for file systems
//!   where native events are unavailable or unreliable.
//!
//! Both start from a snapshot the caller kept from the previous run, and their
//! first poll rescans the whole tree, so changes made while nothing was
//! watching are reported like any other.
//!
//! Scans never trust a path spelling or follow a link. They walk the tree
//! through directory handles opened without following symbolic links, take
//! names from directory listings rather than from event paths, and check that
//! each directory they enter is the one they listed. A scan that sees its root
//! disappear or a directory change under it publishes nothing.
//!
//! A scanner in userspace cannot take an atomic snapshot of a tree that keeps
//! changing, so scans are eventually consistent: once the tree is quiet, a
//! rescan reports it exactly. While it changes, a removal is confirmed rather
//! than inferred. A path the scan saw disappear, as a removal or as the source
//! of a rename, is published only if, at its confirmation, its parent folder,
//! reached from the root without following links, does not list that exact
//! name holding an entry of the old kind. A final pass then opens each such
//! parent again from a fresh root handle and requires every folder on the way
//! to keep its identity, and the parent its modification time and listing. A
//! path that is present, cannot be read, or whose parent changed in between
//! keeps its old entry and is returned for another rescan. So do both ends of
//! a move the scan read halfway: an entry the scan found at two paths of one
//! volume of which one no longer holds it, and an entry about to be published
//! gone that a folder the scan listed holds, at a name that is not another of
//! its links, after its listing changed. An old entry kept back can put its
//! identity at a second path, or leave the identity that path held at none,
//! so those paths keep their old entries too, until neither happens.
//!
//! A rescan of part of the tree reports a move whose other end lies outside
//! the paths it was given as a rescan of both places would. On macOS, for an
//! entry it no longer finds, it asks the system where that identity is now.
//! An entry whose identity the system places where the scan cannot read keeps
//! its old entry until a rescan finds it or the system no longer places it
//! there. An entry whose whereabouts the system cannot tell is kept once, and
//! the next rescan of its path reports it as a rename or as removed.
//! [`NativeWatcher`] keeps every removal that neither a move nor the rules
//! explain the same way and ends the poll, so a file replaced at its path in
//! two steps reads as modified, and a move whose two ends reach it in
//! different polls reads as a rename.
//!
//! A confirmation is a point in time, and these windows remain:
//!
//! - Siblings are confirmed against one listing of their parent, and the final
//!   pass looks at the parent once more. A change undone between the two, such
//!   as a folder swapped out and back or a name created and removed again, is
//!   not seen; neither is one the parent's listing and modification time do
//!   not show.
//! - After the final pass, a removed path can reappear before the caller acts
//!   on the change. Recovering from that is the caller's job: the sync engine's
//!   mass-delete guard and the server trash.
//! - Replacing a file with a folder, or a folder with a file, is reported as
//!   the old entry removed and the new one created at the same path.
//!
//! Scans and watchers given [`Rules`] also leave out what the rules name
//! (design §5.10): the built-in platform files, and every entry a `.koiignore`
//! or `.gitignore` file or the device's own list excludes, with everything
//! below it. A scan reads the rules files ([`Source`]) of each folder it lists
//! and hands the bytes to the caller's matcher ([`Ignore`]) before it judges
//! the folder's entries.
//! Being left out is never a removal. An entry a changed rule leaves out
//! leaves the snapshot without a change, and one a changed rule includes
//! again is reported as created. While the entry stays on disk, the snapshot
//! keeps it in [`Snapshot::left_out`] without what is below it, and an entry
//! there that leaves the disk is reported as removed, or as renamed when the
//! scan finds its identity at an included path. An entry that disappeared is
//! looked for by identity at and below the entries a rule leaves out, with
//! listings and stats only. Found there, it was moved and nothing was
//! deleted, so it is reported as [`Change::Ignored`]. A miss is reported as a
//! removal, although a file saved again after such a move has a new identity
//! and is missed. A miss of a search that ended before it looked everywhere
//! ([`Rules::end_search`]), or that could not open a left-out folder
//! ([`Rules::coverage`]), is reported as [`Change::Missing`], which design
//! §5.10 holds for a question to the user. The search does not look in
//! [`PRIVATE_DIR`] or in the built-in platform files.
//!
//! The engine's private folder [`PRIVATE_DIR`] at the root is never scanned.
//! The [`check_root`] of design §6.2, the root [`Marker`] and the rename that
//! never replaces its target ([`rename_no_replace`], [`probe_no_replace`])
//! live next to the scans.
//!
//! Effects change the disk through a [`Space`]: a descriptor of the root from
//! which every [`Location`] is resolved one folder at a time, without following
//! links and checking each folder's identity, and resolved again before each
//! rename ([`Space::recheck`]). The caller binds the space to a descriptor it
//! already holds ([`Space::bind`]), and the space changes nothing once the path
//! the root was linked at names another folder or nothing ([`Space::held`]).
//! Displaced entries go to the local trash ([`Space::trash`]), which
//! [`Space::reclaim_trash`] empties by age and size, turning an entry written
//! to after it entered into a recovered file ([`recovered_name`]).
//!
//! A [`Section`] carries out [`Effect`]s with the quarantine protocol of
//! design §5.2, writing the root marker first and calling a hook at every
//! [`Step`], and recovers the effects in flight after a crash (design §5.3).
//! Its outcomes come out once every step is durable ([`Section::finish`]).
//!
//! Symbolic links and special files are not synced in v0 (spec §5.1) and are
//! skipped.

use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use notify::event::{AccessKind, AccessMode, EventKind, MetadataKind, ModifyKind};
use notify::{RecommendedWatcher, RecursiveMode};
use rustix::fs::{FileType, Mode, OFlags, RawMode};

/// How long native events must be quiet before the watcher rescans, so both
/// halves of a rename and a create followed by a write land in one batch.
const SETTLE: Duration = Duration::from_millis(50);
/// The longest a batch waits for quiet under constant activity.
const MAX_SETTLE: Duration = Duration::from_secs(1);
const REMOUNT_CHECK: Duration = Duration::from_secs(1);
/// A sync cookie, as in <https://meta.github.io/watchman/docs/cookies>.
const BARRIER: &str = "watch-barrier";
const BARRIER_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("file system watcher: {0}")]
    Watch(Box<dyn std::error::Error + Send + Sync>),
    /// A directory was replaced or removed while the scan was inside it. The
    /// snapshot is unchanged; scan again once the tree is quiet.
    #[error("the tree changed during the scan")]
    Changed,
    /// The root path now names a different directory than the one scanned.
    #[error("the root directory was replaced during the scan")]
    RootReplaced,
    /// The watcher's caller stopped while a scan was walking the tree. The
    /// snapshot is unchanged.
    #[error("the scan stopped because its caller stopped")]
    Stopped,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryKind {
    File,
    Dir,
}

/// What survives a rename and changes when a path is deleted and created
/// again, within the volume the space's root check vouches for. The birth time
/// and the handle guard against inode numbers being reused, on file systems
/// that report them. `ino` is 128 bits wide because ReFS file IDs are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct FileId {
    pub ino: u128,
    pub birth_ns: Option<i64>,
    /// A digest of the handle Linux gives the entry through `name_to_handle_at`, on file systems that give one. The
    /// handle holds the inode's generation, which changes when the inode number is reused, so it tells a new entry
    /// from a deleted one whose inode number and birth time it took within one tick of the kernel's coarse clock.
    pub handle: Option<u64>,
}

impl FileId {
    /// An identity a store or record kept, read back. Only a stat of the entry gives one for an entry on disk.
    pub fn stored(ino: u128, birth_ns: Option<i64>, handle: Option<u64>) -> FileId {
        FileId {
            ino,
            birth_ns,
            handle,
        }
    }

    /// Whether `self` and `other` can be one entry: the same inode and birth time, and the same handle when both
    /// have one. An identity a store kept before it kept handles has none, and holds every entry with its inode and
    /// birth time.
    pub fn same(&self, other: &FileId) -> bool {
        self.ino == other.ino
            && self.birth_ns == other.birth_ns
            && (self.handle.is_none() || other.handle.is_none() || self.handle == other.handle)
    }

    pub(crate) fn without_handle(self) -> FileId {
        FileId {
            handle: None,
            ..self
        }
    }
}

/// One file or folder. For folders, `size`, `mtime_ns` and `executable` are
/// always zero: a folder changes only through its children.
#[derive(Debug, Clone, Copy)]
pub struct Entry {
    pub kind: EntryKind,
    pub id: FileId,
    pub size: u64,
    pub mtime_ns: i128,
    pub ctime: Ctime,
    pub executable: bool,
    /// Whether the file may have another link: it had one when it was
    /// scanned, or it was not scanned. Two entries that differ only here are
    /// equal, since a link made or removed elsewhere changes nothing at this
    /// path.
    pub linked: bool,
    /// The volume mounted at the folder, when it is the root of a volume mounted inside the tree: on another device
    /// than the folder that holds it when it was scanned.
    pub mount: Option<Mount>,
}

/// When a file's metadata last changed. A write always moves it, and no process can set it back, but a write within
/// the same tick of the file system's clock keeps it. A chmod, a link made or removed, or an extended attribute moves
/// it too. Folders have `Settled { at: 0, mode: 0 }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ctime {
    /// Read once its tick had passed, so any later write moves it, with the permission bits the file had then.
    Settled { at: i128, mode: u32 },
    /// Read while its tick lasted, so a later write can keep it: nothing read from the file can be trusted yet. A
    /// scan that finds it unchanged at `until` or later, by this machine's clock, settles it.
    Racy { at: i128, mode: u32, until: i128 },
    /// Not known, but no later than this: a file whose change time is not later counts as unchanged. A file with
    /// several links has `NotAfter(i128::MAX)`, since a link made, moved or removed elsewhere moves its change time,
    /// and no change time it has once it is left with one link matches that.
    NotAfter(i128),
}

impl Ctime {
    /// The change time, or the latest it can be.
    pub fn at(self) -> i128 {
        match self {
            Ctime::Settled { at, .. } | Ctime::Racy { at, .. } | Ctime::NotAfter(at) => at,
        }
    }
}

/// The volume mounted at a folder inside the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mount {
    /// A volume with this UUID.
    Volume(VolumeId),
    /// A volume that has no UUID the system reports, so no scan can tell it from another volume.
    Unidentified,
}

/// Why the scans keep what a snapshot knew at and below a folder and do not read there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detached {
    /// The volume the snapshot knew at the mount point is not mounted there: it was ejected, another volume took its
    /// place, or the volume there has no ID that shows it is the same one.
    Away,
    /// A volume is mounted over a folder whose entries the snapshot knew, which it hides.
    Covered,
}

fn left(dir: BorrowedFd<'_>, name: &OsStr, mount_root: BorrowedFd<'_>) -> bool {
    match (stat_at(dir, name), stat_fd(mount_root)) {
        (Ok(now), Ok(held)) => !now.is_at(held.dev, &held.id),
        _ => true,
    }
}

fn mount_at(dir: BorrowedFd<'_>) -> (Mount, Option<VolumeId>) {
    match root::volume_ids(dir) {
        Some((id, plain)) => (Mount::Volume(id), Some(plain)),
        None => (Mount::Unidentified, None),
    }
}

impl Entry {
    fn is_back(&self, now: &Entry, plain: Option<VolumeId>) -> bool {
        match self.mount {
            Some(Mount::Volume(known)) => {
                let fat = plain.is_some_and(|plain| now.mount != Some(Mount::Volume(plain)));
                (now.mount == Some(Mount::Volume(known)) || plain == Some(known))
                    && self.id.ino == now.id.ino
                    && (self.id.birth_ns == now.id.birth_ns || fat && now.id.birth_ns.is_none())
            }
            Some(Mount::Unidentified) => matches!(now.mount, Some(Mount::Volume(_))),
            None => now.mount.is_none(),
        }
    }

    fn detached(&self) -> Detached {
        match self.mount {
            Some(_) => Detached::Away,
            None => Detached::Covered,
        }
    }

    fn same_version(&self, now: &Entry) -> bool {
        self == now
            && match (self.ctime, now.ctime) {
                (_, Ctime::NotAfter(_)) => true,
                (Ctime::NotAfter(i128::MAX), _) => false,
                (Ctime::NotAfter(at), now) => now.at() <= at,
                (
                    Ctime::Settled { at, mode } | Ctime::Racy { at, mode, .. },
                    Ctime::Settled {
                        at: now_at,
                        mode: now_mode,
                    }
                    | Ctime::Racy {
                        at: now_at,
                        mode: now_mode,
                        ..
                    },
                ) => at == now_at || mode != now_mode,
            }
    }
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.id.same(&other.id)
            && (self.kind, self.size, self.mtime_ns, self.executable)
                == (other.kind, other.size, other.mtime_ns, other.executable)
    }
}

impl Eq for Entry {}

/// An entry a caller of [`Snapshot::reset`] believes exists: its kind and
/// identity, and nothing about what a file holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Known {
    pub kind: EntryKind,
    pub id: FileId,
}

/// A modification time no scan reports, so a file reset to it differs from
/// every scan of it.
const UNVERIFIED: i128 = i128::MIN;

/// What `stat` reports about one directory entry, before deciding whether it is
/// something the snapshot records.
struct Stat {
    file_type: FileType,
    id: FileId,
    dev: u64,
    size: u64,
    mtime_ns: i128,
    /// When the entry's metadata last changed; no process can set it back.
    ctime_ns: i128,
    mode: u32,
    nlink: u64,
}

impl Stat {
    fn entry(&self) -> Option<Entry> {
        Some(match self.file_type {
            FileType::Directory => Entry {
                kind: EntryKind::Dir,
                id: self.id,
                size: 0,
                mtime_ns: 0,
                ctime: Ctime::Settled { at: 0, mode: 0 },
                executable: false,
                linked: false,
                mount: None,
            },
            FileType::RegularFile => Entry {
                kind: EntryKind::File,
                id: self.id,
                size: self.size,
                mtime_ns: self.mtime_ns,
                ctime: match self.nlink {
                    0 | 1 => Ctime::Settled {
                        at: self.ctime_ns,
                        mode: self.mode & 0o7777,
                    },
                    _ => Ctime::NotAfter(i128::MAX),
                },
                executable: self.mode & 0o111 != 0,
                linked: self.nlink > 1,
                mount: None,
            },
            _ => return None,
        })
    }

    fn is_at(&self, dev: u64, id: &FileId) -> bool {
        self.dev == dev && self.id.ino == id.ino
    }
}

fn tick_of(at: i128, measured: Option<i128>) -> i128 {
    let tick = measured.unwrap_or(COARSE_TICK);
    match at % 1_000_000_000 {
        0 => tick.max(SECOND_TICK),
        _ => tick,
    }
}

fn now_ns() -> i128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos() as i128)
}

#[cfg(not(target_os = "linux"))]
const APFS_TICK: i128 = 1_000_000;
const KERNEL_TICK: i128 = 20_000_000;
const SECOND_TICK: i128 = 1_000_000_000 + KERNEL_TICK;
const COARSE_TICK: i128 = 2_000_000_000 + KERNEL_TICK;

/// How far apart two writes to a file on the volume of `dir` must be for the second to get a later change time.
/// APFS stamps writes to the microsecond or finer. Linux stamps them with a clock that ticks every few milliseconds,
/// and exFAT keeps 10 ms. FAT keeps 2 s, HFS+ whole seconds, and a network file system takes the time from another
/// machine; their stamps come from the kernel's coarse clock too, which lags the system clock by up to its tick.
#[cfg(target_os = "linux")]
fn timestamp_tick(dir: BorrowedFd<'_>) -> i128 {
    const EXT: u64 = 0xEF53;
    const XFS: u64 = 0x5846_5342;
    const BTRFS: u64 = 0x9123_683E;
    const TMPFS: u64 = 0x0102_1994;
    const OVERLAYFS: u64 = 0x794C_7630;
    const F2FS: u64 = 0xF2F5_2010;
    const ZFS: u64 = 0x2FC1_2FC1;
    const EXFAT: u64 = 0x2011_BAB0;
    match rustix::fs::fstatfs(dir) {
        Ok(volume)
            if [EXT, XFS, BTRFS, TMPFS, OVERLAYFS, F2FS, ZFS, EXFAT]
                .contains(&(volume.f_type as u64)) =>
        {
            KERNEL_TICK
        }
        _ => COARSE_TICK,
    }
}

#[cfg(not(target_os = "linux"))]
fn timestamp_tick(dir: BorrowedFd<'_>) -> i128 {
    let Ok(volume) = rustix::fs::fstatfs(dir) else {
        return COARSE_TICK;
    };
    let name: Vec<u8> = (volume.f_fstypename.iter())
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect();
    match name.as_slice() {
        b"apfs" => APFS_TICK,
        b"exfat" => KERNEL_TICK,
        _ => COARSE_TICK,
    }
}

#[cfg(target_os = "linux")]
fn statx(dir: BorrowedFd<'_>, name: &OsStr, flags: rustix::fs::AtFlags) -> io::Result<Stat> {
    use rustix::fs::StatxFlags;
    let mask = StatxFlags::BASIC_STATS | StatxFlags::BTIME;
    let nanos = |sec: i64, nsec: u32| i128::from(sec) * 1_000_000_000 + i128::from(nsec);
    let st = rustix::fs::statx(dir, name, flags, mask)?;
    let handle_of = handle(dir, name, flags)?;
    let mount_root = (st.stx_attributes & st.stx_attributes_mask)
        .contains(rustix::fs::StatxAttributes::MOUNT_ROOT);
    let birth_ns = (st.stx_mask & StatxFlags::BTIME.bits() != 0
        && !(mount_root && root::has_no_root_birth(dir, name, flags)))
    .then(|| i64::try_from(nanos(st.stx_btime.tv_sec, st.stx_btime.tv_nsec)).ok())
    .flatten();
    Ok(Stat {
        file_type: FileType::from_raw_mode(RawMode::from(st.stx_mode)),
        id: FileId {
            ino: st.stx_ino.into(),
            birth_ns,
            handle: handle_of,
        },
        dev: rustix::fs::makedev(st.stx_dev_major, st.stx_dev_minor),
        size: st.stx_size,
        mtime_ns: nanos(st.stx_mtime.tv_sec, st.stx_mtime.tv_nsec),
        ctime_ns: nanos(st.stx_ctime.tv_sec, st.stx_ctime.tv_nsec),
        mode: u32::from(st.stx_mode),
        nlink: u64::from(st.stx_nlink),
    })
}

#[cfg(target_os = "linux")]
fn handle(
    dir: BorrowedFd<'_>,
    name: &OsStr,
    flags: rustix::fs::AtFlags,
) -> io::Result<Option<u64>> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(name.as_bytes()).map_err(io::Error::other)?;
    let at = if flags.contains(rustix::fs::AtFlags::EMPTY_PATH) {
        libc::AT_EMPTY_PATH
    } else if flags.contains(rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
        0
    } else {
        libc::AT_SYMLINK_FOLLOW
    };
    use std::os::fd::AsRawFd;
    use std::sync::atomic::{AtomicBool, Ordering};
    static FID_REFUSED: AtomicBool = AtomicBool::new(false);
    #[repr(C)]
    struct Handle {
        len: libc::c_uint,
        kind: libc::c_int,
        bytes: [u8; libc::MAX_HANDLE_SZ as usize],
    }
    let mut handle = Handle {
        len: libc::MAX_HANDLE_SZ as libc::c_uint,
        kind: 0,
        bytes: [0; libc::MAX_HANDLE_SZ as usize],
    };
    let mut mount: libc::c_int = 0;
    let fid = !FID_REFUSED.load(Ordering::Relaxed);
    let done = unsafe {
        libc::syscall(
            libc::SYS_name_to_handle_at,
            dir.as_raw_fd(),
            path.as_ptr(),
            &raw mut handle,
            &raw mut mount,
            at | if fid { libc::AT_HANDLE_FID } else { 0 },
        )
    };
    if done != 0 {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::EINVAL) if fid => {
                FID_REFUSED.store(true, Ordering::Relaxed);
                crate::handle(dir, name, flags)
            }
            Some(libc::EOPNOTSUPP | libc::EOVERFLOW) => Ok(None),
            _ => Err(error),
        };
    }
    let fnv_1a = |hash: u64, byte: &u8| (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
    Ok(Some(
        (handle.kind.to_le_bytes().iter())
            .chain(&handle.bytes[..handle.len as usize])
            .fold(0xcbf2_9ce4_8422_2325, fnv_1a),
    ))
}

/// Stats `name` inside `dir` without following a symbolic link.
#[cfg(target_os = "linux")]
fn stat_at(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<Stat> {
    statx(dir, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
}

/// The identity of the open file or folder `fd`, as a scan records it.
pub fn id_of(fd: BorrowedFd<'_>) -> io::Result<FileId> {
    Ok(stat_fd(fd)?.id)
}

#[cfg(target_os = "linux")]
fn stat_fd(fd: BorrowedFd<'_>) -> io::Result<Stat> {
    statx(fd, OsStr::new(""), rustix::fs::AtFlags::EMPTY_PATH)
}

/// Stats `path`, following symbolic links.
#[cfg(target_os = "linux")]
fn stat_path(path: &Path) -> io::Result<Stat> {
    statx(
        rustix::fs::CWD,
        path.as_os_str(),
        rustix::fs::AtFlags::empty(),
    )
}

#[cfg(not(target_os = "linux"))]
fn from_stat(st: rustix::fs::Stat) -> Stat {
    let nanos = |sec: i64, nsec: i64| i128::from(sec) * 1_000_000_000 + i128::from(nsec);
    Stat {
        file_type: FileType::from_raw_mode(RawMode::from(st.st_mode)),
        id: FileId {
            ino: st.st_ino.into(),
            birth_ns: i64::try_from(nanos(st.st_birthtime, st.st_birthtime_nsec)).ok(),
            handle: None,
        },
        dev: st.st_dev as u64,
        size: st.st_size as u64,
        mtime_ns: nanos(st.st_mtime, st.st_mtime_nsec),
        ctime_ns: nanos(st.st_ctime, st.st_ctime_nsec),
        mode: u32::from(st.st_mode),
        nlink: u64::from(st.st_nlink),
    }
}

/// Stats `name` inside `dir` without following a symbolic link.
#[cfg(not(target_os = "linux"))]
fn stat_at(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<Stat> {
    Ok(from_stat(rustix::fs::statat(
        dir,
        name,
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )?))
}

#[cfg(not(target_os = "linux"))]
fn stat_fd(fd: BorrowedFd<'_>) -> io::Result<Stat> {
    Ok(from_stat(rustix::fs::fstat(fd)?))
}

/// Stats `path`, following symbolic links.
#[cfg(not(target_os = "linux"))]
fn stat_path(path: &Path) -> io::Result<Stat> {
    Ok(from_stat(rustix::fs::stat(path)?))
}

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

fn open_root(root: &Path) -> io::Result<OwnedFd> {
    Ok(rustix::fs::open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}

/// Opens the directory `name` inside `dir`, refusing to follow a symbolic link.
fn open_dir_at(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<OwnedFd> {
    Ok(rustix::fs::openat(dir, name, DIR_FLAGS, Mode::empty())?)
}

/// Whether `name` in the folder at `parent` is the engine's private folder.
fn private(parent: &Path, name: &OsStr) -> bool {
    parent.as_os_str().is_empty() && name == PRIVATE_DIR
}

/// Whether the error means the scan may not read there.
fn denied(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::PermissionDenied
}

/// Whether the error means the name no longer leads to a directory: it is gone,
/// or a file or symbolic link took its place.
fn is_gone(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    ) || err.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error())
}

/// A walk of the scopes of a folder for the entries with some kinds and identities, carried on by [`Search::step`]
/// within a budget. Each entry is stat'ed where its folder lists it, so no file is opened and no symbolic link is
/// followed, and one folder is open at a time. A walk ends without an entry only once no folder it listed has changed
/// since.
#[derive(Debug)]
pub struct Search {
    wanted: Vec<Known>,
    hits: Vec<Option<PathBuf>>,
    covered: Vec<PathBuf>,
    scopes: Vec<PathBuf>,
    folders: Vec<PathBuf>,
    listed: BTreeMap<PathBuf, ([i128; 2], i128)>,
    ticks: HashMap<u64, i128>,
}

impl Search {
    pub fn new(wanted: Vec<Known>) -> Search {
        Search {
            hits: vec![None; wanted.len()],
            wanted,
            covered: Vec::new(),
            scopes: Vec::new(),
            folders: Vec::new(),
            listed: BTreeMap::new(),
            ticks: HashMap::new(),
        }
    }

    /// Looks at up to `budget` more entries, taking them from it, at or below `scopes` of the folder at `root`, adding
    /// each scope that no earlier step covered. Returns where each wanted entry is, in order, once the walk has found
    /// them all or ended, and `None` when the budget ran out or `stopped` held first. Fails when a folder there cannot
    /// be listed, since an entry may be in it.
    pub fn step(
        &mut self,
        root: &Path,
        scopes: &[&PathBuf],
        budget: &mut usize,
        stopped: &dyn Fn() -> bool,
    ) -> io::Result<Option<Vec<Option<PathBuf>>>> {
        for scope in scopes {
            if !(self.covered.iter()).any(|covered| scope.starts_with(covered)) {
                self.covered.push((*scope).clone());
                self.scopes.push((*scope).clone());
            }
        }
        let root = open_root(root)?;
        while let Some(scope) = self.scopes.pop() {
            let (Some(parent), Some(name)) = (scope.parent(), scope.file_name()) else {
                self.folders.push(scope);
                continue;
            };
            let Some((dir, _)) = open_attached(root.as_fd(), parent, &mut |_| {})? else {
                continue;
            };
            let stat = match stat_at(dir.as_fd(), name) {
                Err(err) if is_gone(&err) => continue,
                stat => stat?,
            };
            if self.hit(&stat, &scope) {
                return Ok(Some(std::mem::take(&mut self.hits)));
            }
            if stat.file_type == FileType::Directory {
                self.folders.push(scope);
            }
        }
        while let Some(folder) = self.folders.pop() {
            if *budget == 0 || stopped() {
                self.folders.push(folder);
                return Ok(None);
            }
            let Some((dir, _)) = open_attached(root.as_fd(), &folder, &mut |_| {})? else {
                continue;
            };
            let seen = now_ns();
            let bound = stat_fd(dir.as_fd())?;
            (self.listed).insert(folder.clone(), ([bound.mtime_ns, bound.ctime_ns], seen));
            for name in list(dir.as_fd())? {
                *budget = budget.saturating_sub(1);
                let stat = match stat_at(dir.as_fd(), &name) {
                    Err(err) if is_gone(&err) => continue,
                    stat => stat?,
                };
                let at = folder.join(&name);
                if self.hit(&stat, &at) {
                    return Ok(Some(std::mem::take(&mut self.hits)));
                }
                if stat.file_type == FileType::Directory && !self.listed.contains_key(&at) {
                    self.folders.push(at);
                }
            }
        }
        for (folder, (times, seen)) in std::mem::take(&mut self.listed) {
            let Some((dir, _)) = open_attached(root.as_fd(), &folder, &mut |_| {})? else {
                continue;
            };
            let now = stat_fd(dir.as_fd())?;
            let tick = *(self.ticks)
                .entry(now.dev)
                .or_insert_with(|| timestamp_tick(dir.as_fd()));
            if [now.mtime_ns, now.ctime_ns] == times
                && (times.iter()).all(|at| seen >= at + tick_of(*at, Some(tick)))
            {
                self.listed.insert(folder, (times, seen));
            } else {
                self.folders.push(folder);
            }
        }
        if !self.folders.is_empty() {
            return Ok(None);
        }
        Ok(Some(std::mem::take(&mut self.hits)))
    }

    fn hit(&mut self, stat: &Stat, at: &Path) -> bool {
        if let Some(entry) = stat.entry() {
            for (known, hit) in self.wanted.iter().zip(&mut self.hits) {
                if hit.is_none() && entry.kind == known.kind && entry.id.same(&known.id) {
                    *hit = Some(at.to_path_buf());
                }
            }
        }
        self.hits.iter().all(Option::is_some)
    }
}

fn list(dir: BorrowedFd<'_>) -> io::Result<BTreeSet<OsString>> {
    let mut names = BTreeSet::new();
    for item in rustix::fs::Dir::read_from(dir)? {
        let name = item?.file_name().to_bytes().to_vec();
        if name != b"." && name != b".." {
            names.insert(OsString::from(OsStr::from_bytes(&name)));
        }
    }
    Ok(names)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Keep<'a> {
    Unplaced,
    Unexplained { waiting: &'a BTreeSet<PathBuf> },
}

/// Where a scan can be interrupted in tests.
#[cfg_attr(not(test), allow(dead_code))]
enum Cut<'a> {
    RootOpened,
    OpeningDir(&'a Path),
    /// A removal's confirmation just opened this folder.
    Confirming(&'a Path),
    Searching(&'a Path),
    Stating(&'a Path),
}

/// The entries one rescan observed, and the parts of the snapshot they replace.
struct Scan<'c> {
    new: BTreeMap<PathBuf, Entry>,
    /// Every folder the scan opened, with the identity it had when opened.
    /// Each must still be at its path when the scan is published.
    bindings: Vec<(PathBuf, u64, FileId)>,
    /// Paths whose old entry and everything below it give way to `new`.
    replaced: Vec<PathBuf>,
    /// Folders the scan may not open or list. What lies below them stays as
    /// it was known.
    unreadable: Vec<PathBuf>,
    left_out: BTreeMap<PathBuf, Entry>,
    mounts: BTreeMap<PathBuf, Entry>,
    plain_ids: BTreeMap<PathBuf, VolumeId>,
    detached: Vec<(PathBuf, Detached)>,
    /// The rules that leave entries out, for a scan that applies any.
    rules: Option<Pending<'c>>,
    cut: &'c mut dyn FnMut(Cut<'_>),
    /// Whether the caller has stopped. The scan fails with [`Error::Stopped`]
    /// at the next entry once it holds.
    stopped: &'c dyn Fn() -> bool,
    files: u64,
    found: &'c dyn Fn(u64),
    ticks: HashMap<u64, i128>,
    racy: BTreeMap<PathBuf, Ctime>,
    marks: &'c MarkLog,
    listed: Vec<(PathBuf, [i128; 2], i128)>,
}

impl Scan<'_> {
    fn settle(
        &mut self,
        dir: (BorrowedFd<'_>, u64),
        dev: u64,
        rel: &Path,
        ctime: Ctime,
        seen: i128,
    ) -> Ctime {
        let Ctime::Settled { at, mode } = ctime else {
            return ctime;
        };
        let measured = (dir.1 == dev).then(|| {
            *self
                .ticks
                .entry(dev)
                .or_insert_with(|| timestamp_tick(dir.0))
        });
        let until = match self.racy.get(rel) {
            Some(&Ctime::Racy { at: was, until, .. }) if was == at => until,
            _ => at.min(seen) + tick_of(at, measured),
        };
        match seen >= until {
            true => ctime,
            false => Ctime::Racy { at, mode, until },
        }
    }

    /// Records the entry `name` of `dir` at `rel`, and everything below it.
    fn entry(&mut self, dir: BorrowedFd<'_>, dev: u64, name: &OsStr, rel: PathBuf) -> Result<()> {
        if (self.stopped)() {
            return Err(Error::Stopped);
        }
        (self.cut)(Cut::Stating(&rel));
        let seen = now_ns();
        let stat = match stat_at(dir, name) {
            Ok(stat) => stat,
            Err(err) if is_gone(&err) => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        let Some(mut entry) = stat.entry() else {
            return Ok(());
        };
        let mut mount_root = None;
        let mut locked = false;
        if entry.kind == EntryKind::File {
            entry.ctime = self.settle((dir, dev), stat.dev, &rel, entry.ctime, seen);
        }
        if entry.kind == EntryKind::Dir && stat.dev != dev {
            match open_dir_at(dir, name) {
                Ok(sub) => {
                    if !stat_fd(sub.as_fd())?.is_at(stat.dev, &entry.id) {
                        return Err(Error::Changed);
                    }
                    let (mount, plain) = mount_at(sub.as_fd());
                    entry.mount = Some(mount);
                    if let Some(plain) = plain {
                        self.plain_ids.insert(rel.clone(), plain);
                    }
                    mount_root = Some(sub);
                }
                Err(err) if is_gone(&err) => return Err(Error::Changed),
                Err(err) if denied(&err) => locked = true,
                Err(err) => return Err(err.into()),
            }
        }
        if let Some(rules) = &mut self.rules
            && rules.leaves_out(&rel, entry.kind)
        {
            self.left_out.insert(rel, entry);
            return Ok(());
        }
        if locked {
            self.unreadable.push(rel);
            return Ok(());
        }
        let held = match self.mounts.get(&rel) {
            Some(known) => {
                (!known.is_back(&entry, self.plain_ids.get(&rel).copied())).then_some(*known)
            }
            None => (entry.mount == Some(Mount::Unidentified)).then_some(entry),
        };
        if let Some(held) = held {
            if mount_root
                .as_ref()
                .is_some_and(|sub| left(dir, name, sub.as_fd()))
            {
                return Err(Error::Changed);
            }
            self.new.insert(rel.clone(), held);
            self.detached.push((rel.clone(), held.detached()));
            self.unreadable.push(rel);
            return Ok(());
        }
        self.new.insert(rel.clone(), entry);
        if entry.kind == EntryKind::File {
            self.files += 1;
            (self.found)(self.files);
            return Ok(());
        }
        (self.cut)(Cut::OpeningDir(&rel));
        let crossed = mount_root.is_some();
        let sub = match mount_root.map_or_else(|| open_dir_at(dir, name), Ok) {
            Ok(sub) => sub,
            Err(err) if is_gone(&err) => return Err(Error::Changed),
            Err(err) if denied(&err) => {
                self.unreadable.push(rel);
                return Ok(());
            }
            Err(err) => return Err(err.into()),
        };
        let result = stat_fd(sub.as_fd()).map_err(Error::from).and_then(|bound| {
            if !bound.is_at(stat.dev, &entry.id) {
                return Err(Error::Changed);
            }
            self.bindings.push((rel.clone(), bound.dev, bound.id));
            (self.listed).push((rel.clone(), [bound.mtime_ns, bound.ctime_ns], seen));
            self.dir(sub.as_fd(), bound.dev, &rel)
        });
        match result {
            Err(Error::Io(_)) if crossed && left(dir, name, sub.as_fd()) => Err(Error::Changed),
            other => other,
        }
    }

    fn dir(&mut self, dir: BorrowedFd<'_>, dev: u64, rel: &Path) -> Result<()> {
        let result = list(dir).map_err(Error::from).and_then(|names| {
            if self.read_rules(dir, rel, &names) == RulesRead::Unknown {
                self.unreadable.push(rel.to_path_buf());
                return Ok(());
            }
            names
                .iter()
                .filter(|name| !private(rel, name))
                .try_for_each(|name| self.entry(dir, dev, name, rel.join(name)))
        });
        self.unless_denied(rel, result)
    }

    /// Reads the rules files of the folder `dir` at `rel`, which lists `names`,
    /// before any of its children is judged. A folder whose rules are
    /// [`RulesRead::Unknown`] is unreadable: the caller records it and judges none
    /// of its children.
    fn read_rules(
        &mut self,
        dir: BorrowedFd<'_>,
        rel: &Path,
        names: &BTreeSet<OsString>,
    ) -> RulesRead {
        (self.rules.as_mut()).map_or(RulesRead::Same, |rules| rules.read(dir, rel, names))
    }

    /// Records `rel` as unreadable and succeeds when `result` failed because
    /// the scan may not read in that folder.
    fn unless_denied(&mut self, rel: &Path, result: Result<()>) -> Result<()> {
        match result {
            Err(Error::Io(err)) if denied(&err) => {
                self.unreadable.push(rel.to_path_buf());
                Ok(())
            }
            other => other,
        }
    }
}

/// A change between two states of the tree. Paths are relative to the root.
/// `Removed::path`, `Renamed::from` and `Ignored::from` name the previous
/// state; `Created::path`, `Modified::path` and `Renamed::to` name the new one.
/// Resolve previous-state paths before applying any change of the same batch.
///
/// Removing or renaming a folder is one change; its contents are not listed
/// unless they changed on their own. Creating a folder lists every entry in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Created {
        path: PathBuf,
        kind: EntryKind,
    },
    Modified {
        path: PathBuf,
    },
    Renamed {
        from: PathBuf,
        to: PathBuf,
        kind: EntryKind,
    },
    Removed {
        path: PathBuf,
        kind: EntryKind,
    },
    /// The entry at `path`, with identity `id`, left the disk, and the search below the left-out entries did not
    /// look everywhere: it ended early ([`Rules::end_search`]) or could not open a folder ([`Rules::coverage`]). The
    /// entry may still be in a left-out folder.
    Missing {
        path: PathBuf,
        kind: EntryKind,
        id: FileId,
    },
    /// The folder at `path` is another folder on disk that holds the entries the old one held, as after an app
    /// writes a copy of a folder and swaps it in. It is the same folder, with a new identity.
    Reidentified {
        path: PathBuf,
    },
    /// The entry at `from` now sits at `to`, a path the rules leave out. The
    /// snapshot no longer holds it or anything below it. Nothing was deleted.
    Ignored {
        from: PathBuf,
        to: PathBuf,
        kind: EntryKind,
        id: FileId,
    },
}

/// The known state of every entry under a root, keyed by relative path with the
/// exact names the directory listings return.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub entries: BTreeMap<PathBuf, Entry>,
    /// The entries the snapshot knew that the rules now leave out, as each was
    /// last known. What is below them is not known.
    pub left_out: BTreeMap<PathBuf, Entry>,
    /// Mount points whose volume the last scan of each did not find there: unmounted, another volume, or one without
    /// a UUID. The entries keep what was known below each, and no scan reads there until the volume is back.
    pub detached: BTreeMap<PathBuf, Detached>,
    /// Paths where the next scan reports a removal without keeping it: paths a scan saw disappear and kept, because
    /// the system could not tell where their identity went or a [`NativeWatcher`] waits for the other end of a move,
    /// and the scope of a [`Snapshot::reset`]. A [`NativeWatcher`] keeps a path it waits for until it has heard
    /// every event from before the scan that kept it. A kept path keeps what was known at and below it.
    pub unplaced: BTreeSet<PathBuf>,
}

/// What one [`Snapshot::rescan`] found.
#[derive(Debug, PartialEq, Eq)]
pub struct Rescan {
    /// The changes, already applied to the snapshot. A removal held back for
    /// the search under left-out entries leaves the snapshot without one.
    pub changes: Vec<Change>,
    /// Paths the scan saw disappear but could not confirm gone: present,
    /// unreadable, or under a parent that changed during the confirmation.
    /// The snapshot keeps what it knew about them and everything below them.
    /// Also the paths of removals held back while the search under left-out
    /// entries goes on: the snapshot holds nothing there, and the rules hold
    /// the entries until the search decides. Also both ends of a move the
    /// scan read halfway, which keep what the snapshot knew there, nothing at
    /// a new name. Rescan them later.
    pub unsettled: Vec<PathBuf>,
    /// Folders the scan may not open or list, folders with a rules file it
    /// cannot read when no earlier scan read one for them, so that what they
    /// leave out is not known, and mount points the snapshot knows whose
    /// volume is no longer mounted there. The snapshot keeps what it knew at
    /// a mount point and below these folders, so a folder the user cannot read
    /// or a disk the user ejected never reads as emptied, and nothing a rules
    /// file may leave out is reported as new. An entry below such a folder
    /// whose identity the scan found at a readable path as its only link is
    /// reported as moved there, when everything that was below it is found
    /// there, below a folder there the scan cannot read, or elsewhere. Report
    /// them, and rescan them once they can be read.
    pub unreadable: Vec<PathBuf>,
    /// The identity of the root folder the scan opened. A scan that reached
    /// another folder than the caller's root has already applied that folder
    /// to the snapshot, so stop using the snapshot, not only its changes.
    pub root: FileId,
    /// The rules files this scan found changed, each as its folder, its kind
    /// and the bytes it holds now, or `None` for a folder that has none of
    /// that kind any more. Empty for a scan without [`Rules`].
    pub rules: Vec<(PathBuf, Source, Option<Vec<u8>>)>,
    /// The folders whose rules this scan established, at the paths it listed
    /// them: it read their rules files, found none, or could not read one and
    /// kept the rules an earlier scan read. A folder reported unreadable
    /// before has known rules again once it is here, whatever its rules files
    /// did meanwhile. Empty for a scan without [`Rules`].
    pub read: Vec<PathBuf>,
    pub settled: Vec<PathBuf>,
}

impl Snapshot {
    pub fn scan(root: impl AsRef<Path>) -> Result<Self> {
        let mut snapshot = Self::default();
        snapshot.rescan(root, [PathBuf::new()])?;
        Ok(snapshot)
    }

    /// [`Snapshot::scan`], leaving out what `rules` leave out.
    pub fn scan_with_rules(root: impl AsRef<Path>, rules: &mut Rules) -> Result<Self> {
        let mut snapshot = Self::default();
        snapshot.rescan_with_rules(root, [PathBuf::new()], rules)?;
        Ok(snapshot)
    }

    /// Rescans the given relative paths, each with everything below it, updates
    /// the snapshot and returns what changed. An empty path rescans the whole
    /// tree.
    ///
    /// A path is looked up in a listing of its parent, so a spelling the file
    /// system resolves case- or normalization-insensitively counts as absent,
    /// and the listing also reveals entries of that parent the snapshot knows
    /// by a name that is gone. A path whose parent the snapshot does not know
    /// as a folder, or whose parent is no longer that folder, is widened to the
    /// parent.
    ///
    /// On any error the snapshot is unchanged. That includes a root that is
    /// missing, unreadable or replaced at any point of the scan, so a lost root
    /// never reads as everything removed, and [`Error::Changed`] when a folder
    /// is replaced while the scan is inside it.
    ///
    /// A path is only reported removed, or as the source of a rename, once a
    /// fresh lookup from the root confirms it is gone, as the crate docs
    /// describe. Otherwise it keeps its old entry and is returned in
    /// [`Rescan::unsettled`].
    pub fn rescan(
        &mut self,
        root: impl AsRef<Path>,
        paths: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Rescan> {
        self.rescan_with(root.as_ref(), paths, None, &mut |_| {})
    }

    /// [`Snapshot::rescan`], leaving out what `rules` leave out, as the crate
    /// docs describe.
    ///
    /// In each folder it lists, the scan reads the rules files before it judges
    /// the folder's entries, and it returns the folders whose rules changed in
    /// [`Rescan::rules`]. A changed rules file widens the scan to everything
    /// below its folder. An entry the rules now leave out leaves the snapshot
    /// without a change, and one they include again is reported as created.
    /// One of [`Snapshot::left_out`] that is no longer on disk is reported as
    /// removed, or as renamed when the scan finds its identity at an included
    /// path.
    /// A removal whose entry the scan finds under a left-out entry is reported
    /// as [`Change::Ignored`], and one found at such an entry's path is kept
    /// in [`Snapshot::left_out`]. Each rescan looks at a bounded number of
    /// left-out entries ([`Rules::with_search_budget`]). A removal the search
    /// has not decided within that bound is held back: the snapshot drops its
    /// entry without a change, `rules` hold it at the path the renames of this
    /// and later scans move it to, and that path is returned in
    /// [`Rescan::unsettled`]. The search goes on in the next rescans, of any
    /// paths, and the removal is reported by the next rescan of its path after
    /// the search has decided it, as [`Change::Missing`] when the search ended
    /// before it looked everywhere, or as a rename when that rescan finds its
    /// identity at an included path. A file created at its path is held back
    /// with it, and both are reported once the search has decided: as a
    /// modification when the search did not find the removed entry, and as
    /// the removal and then the creation when it did. A rename or a change
    /// onto its path makes the search run to its end, so the removal is
    /// reported first. The first rescan of a path with new `rules` holds
    /// each entry there whose path now holds another identity the same way,
    /// so a removal another watcher held is not read as a replacement; a path
    /// below a folder the rescan cannot read is not counted as scanned. A held entry whose
    /// removal the scan cannot confirm, as below a folder it cannot read,
    /// stays held. One whose path the rules now leave out is left to the
    /// search once its folder can be read, whatever the disk holds at that
    /// path.
    ///
    /// On any error `rules` are unchanged too.
    pub fn rescan_with_rules(
        &mut self,
        root: impl AsRef<Path>,
        paths: impl IntoIterator<Item = PathBuf>,
        rules: &mut Rules,
    ) -> Result<Rescan> {
        self.rescan_under(
            root.as_ref(),
            paths,
            &BTreeSet::new(),
            None,
            Some(rules),
            &|| false,
            &|_| {},
            Keep::Unplaced,
            &mut |_| {},
            &MarkLog::default(),
        )
    }

    /// Replaces what the snapshot knows at and below `scope`, a relative
    /// path, with the entries of `known` there. The next rescan of `scope`
    /// then reports an entry of `known` the disk lacks as removed, one found
    /// at another path as renamed, and an entry `known` lacks as created. It
    /// also reports every file of `known` it finds as modified, changed or
    /// not: `known` says that a file exists, not what it holds, so the caller
    /// reads it again. An empty path covers the whole tree. Entries of
    /// `known` outside `scope` are ignored.
    pub fn reset(&mut self, scope: &Path, known: BTreeMap<PathBuf, Known>) {
        let held: Vec<PathBuf> = self
            .entries
            .range(scope.to_path_buf()..)
            .take_while(|(path, _)| path.starts_with(scope))
            .map(|(path, _)| path.clone())
            .collect();
        let mut mounts = BTreeMap::new();
        for path in held {
            if let Some(entry) = self.entries.remove(&path)
                && let Some(mount) = entry.mount
            {
                mounts.insert(path, (entry.id, mount));
            }
        }
        self.left_out.retain(|path, _| !path.starts_with(scope));
        self.detached.retain(|path, _| !path.starts_with(scope));
        self.unplaced.retain(|path| !path.starts_with(scope));
        self.unplaced.insert(scope.to_path_buf());
        self.entries.extend(
            known
                .into_iter()
                .filter(|(path, _)| path.starts_with(scope))
                .map(|(path, Known { kind, id })| {
                    let mtime_ns = match kind {
                        EntryKind::File => UNVERIFIED,
                        EntryKind::Dir => 0,
                    };
                    let entry = Entry {
                        kind,
                        id,
                        size: 0,
                        mtime_ns,
                        ctime: Ctime::Settled { at: 0, mode: 0 },
                        executable: false,
                        linked: kind == EntryKind::File,
                        mount: (mounts.get(&path))
                            .filter(|(was, _)| was.same(&id))
                            .map(|(_, mount)| *mount),
                    };
                    (path, entry)
                }),
        );
    }

    fn look_for(
        &self,
        root: &Path,
        rules: Option<&mut Rules>,
        wanted: &BTreeMap<PathBuf, Known>,
        found: &BTreeMap<PathBuf, PathBuf>,
    ) -> Result<Vec<(PathBuf, Looked)>> {
        let Some(rules) = rules else {
            return Ok(Vec::new());
        };
        let mut rules = Pending::new(rules);
        rules.seek(
            wanted
                .values()
                .map(|known| (known.id, known.kind))
                .collect(),
        );
        if wanted.is_empty() {
            return Ok(Vec::new());
        }
        let root = open_root(root)?;
        let entry_at = |path: &Path| -> io::Result<Option<Entry>> {
            let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
                return Ok(None);
            };
            let Some((dir, _)) = open_attached(root.as_fd(), parent, &mut |_| {})? else {
                return Ok(None);
            };
            match stat_at(dir.as_fd(), name) {
                Ok(stat) => Ok(stat.entry()),
                Err(err) if is_gone(&err) => Ok(None),
                Err(err) => Err(err),
            }
        };
        let mut looked = Vec::new();
        let mut search = Wanted::new();
        for (path, known) in wanted {
            match found.get(path).map(|at| entry_at(at)) {
                Some(Err(_)) => {}
                Some(Ok(Some(entry))) if entry.kind == known.kind && !entry.id.same(&known.id) => {
                    looked.push((path.clone(), Looked::Replaced(entry.id)));
                }
                _ => {
                    search.insert((known.id, known.kind), None);
                }
            }
        }
        let fresh: HashSet<Key> = (search.keys().copied())
            .filter(|key| rules.unproven(*key))
            .collect();
        let undecided = rules.search(root.as_fd(), &mut search, &fresh, true, &mut |_| {});
        looked.extend((wanted.iter()).filter_map(|(path, known)| {
            let at = search.get(&(known.id, known.kind))?.clone()?;
            Some((path.clone(), Looked::Found(at)))
        }));
        search.retain(|key, at| at.is_none() && !undecided.contains(key) && !rules.unproven(*key));
        if search.is_empty() {
            return Ok(looked);
        }
        let mut held: HashMap<Key, Vec<FileId>> = HashMap::new();
        for entry in self.entries.values().chain(self.left_out.values()) {
            (held.entry((entry.id.without_handle(), entry.kind)))
                .or_default()
                .push(entry.id);
        }
        looked.extend(
            (wanted.iter())
                .filter(|(_, known)| search.contains_key(&(known.id, known.kind)))
                .filter(|(_, known)| {
                    !(held.get(&(known.id.without_handle(), known.kind)))
                        .is_some_and(|ids| ids.iter().any(|id| id.same(&known.id)))
                })
                .map(|(path, _)| (path.clone(), Looked::Gone)),
        );
        Ok(looked)
    }

    /// [`Snapshot::rescan`], failing with [`Error::RootReplaced`] and
    /// changing nothing when `root` opens a folder other than `expected`.
    fn rescan_with(
        &mut self,
        root: &Path,
        paths: impl IntoIterator<Item = PathBuf>,
        expected: Option<FileId>,
        cut: &mut dyn FnMut(Cut<'_>),
    ) -> Result<Rescan> {
        self.rescan_under(
            root,
            paths,
            &BTreeSet::new(),
            expected,
            None,
            &|| false,
            &|_| {},
            Keep::Unplaced,
            cut,
            &MarkLog::default(),
        )
    }

    /// [`Snapshot::rescan_with`], leaving out what `rules` leave out, and
    /// failing with [`Error::Stopped`] once `stopped` holds. As it walks, it
    /// calls `found` with the number of files it has recorded so far. A scan
    /// that fails takes back what it changed in `rules`.
    #[allow(clippy::too_many_arguments)]
    fn rescan_under(
        &mut self,
        root: &Path,
        paths: impl IntoIterator<Item = PathBuf>,
        listed: &BTreeSet<PathBuf>,
        expected: Option<FileId>,
        mut rules: Option<&mut Rules>,
        stopped: &dyn Fn() -> bool,
        found: &dyn Fn(u64),
        keep: Keep<'_>,
        cut: &mut dyn FnMut(Cut<'_>),
        marks: &MarkLog,
    ) -> Result<Rescan> {
        let mut paths: Vec<PathBuf> = paths.into_iter().collect();
        let mut bound = None;
        let mut widened = false;
        loop {
            let mut scan = Scan {
                new: BTreeMap::new(),
                bindings: Vec::new(),
                listed: Vec::new(),
                replaced: Vec::new(),
                unreadable: Vec::new(),
                left_out: BTreeMap::new(),
                mounts: (self.entries.iter())
                    .filter(|(path, entry)| {
                        entry.mount.is_some()
                            || self.detached.get(*path) == Some(&Detached::Covered)
                    })
                    .map(|(path, entry)| (path.clone(), *entry))
                    .collect(),
                plain_ids: BTreeMap::new(),
                detached: Vec::new(),
                rules: rules.as_deref_mut().map(Pending::new),
                cut: &mut *cut,
                stopped,
                files: 0,
                found,
                ticks: HashMap::new(),
                racy: (self.entries.iter())
                    .filter(|(_, entry)| matches!(entry.ctime, Ctime::Racy { .. }))
                    .map(|(path, entry)| (path.clone(), entry.ctime))
                    .collect(),
                marks,
            };
            let scanned =
                (self.scan_scopes(root, &paths, listed, expected, &mut scan)).and_then(|stat| {
                    match bound {
                        Some((dev, id)) if !stat.is_at(dev, &id) => Err(Error::RootReplaced),
                        _ => Ok(stat),
                    }
                });
            if scanned.is_ok() {
                self.learn_handles(&scan);
            }
            let (unscanned, hidden, unsure) = match &scanned {
                Ok(_) => self.other_ends(root, &scan, &paths),
                Err(_) => (Vec::new(), Vec::new(), Vec::new()),
            };
            let whole = paths.iter().any(|path| path.as_os_str().is_empty());
            if let Ok(stat) = &scanned
                && !unscanned.is_empty()
                && !whole
            {
                bound = Some((stat.dev, stat.id));
                if let Some(rules) = scan.rules {
                    rules.undo();
                }
                paths = if widened {
                    vec![PathBuf::new()]
                } else {
                    [paths, unscanned].concat()
                };
                widened = true;
                continue;
            }
            let result =
                scanned.and_then(|stat| self.publish(root, &stat, &mut scan, hidden, unsure, keep));
            if result.is_err()
                && let Some(rules) = scan.rules
            {
                rules.undo();
            }
            return result;
        }
    }

    fn learn_handles(&mut self, scan: &Scan<'_>) {
        let mut found: HashMap<(FileId, EntryKind), Option<u64>> = HashMap::new();
        for (path, now) in scan.new.iter().chain(&scan.left_out) {
            let at_its_path = (self.entries.get(path))
                .is_some_and(|before| before.id.without_handle() == now.id.without_handle());
            if now.id.handle.is_some() && !at_its_path {
                (found.entry((now.id.without_handle(), now.kind)))
                    .and_modify(|held| {
                        if *held != now.id.handle {
                            *held = None;
                        }
                    })
                    .or_insert(now.id.handle);
            }
        }
        if found.is_empty() {
            return;
        }
        for entry in (self.entries.values_mut()).chain(self.left_out.values_mut()) {
            if entry.id.handle.is_none()
                && let Some(&handle) = found.get(&(entry.id, entry.kind))
            {
                entry.id.handle = handle;
            }
        }
    }

    fn other_ends(
        &self,
        root: &Path,
        scan: &Scan<'_>,
        scopes: &[PathBuf],
    ) -> (Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>) {
        let scanned: HashSet<&Path> = (scan.replaced.iter().chain(scopes))
            .map(PathBuf::as_path)
            .collect();
        let covered = |path: &Path| path.ancestors().any(|above| scanned.contains(above));
        let blocked = |path: &Path| (scan.unreadable.iter()).any(|dir| path.starts_with(dir));
        let same =
            |entry: &Entry, other: &Entry| entry.id.same(&other.id) && entry.kind == other.kind;
        let mut arrived: HashMap<(FileId, EntryKind), Vec<&PathBuf>> = HashMap::new();
        for (path, now) in &scan.new {
            if !(self.entries.get(path)).is_some_and(|before| same(before, now)) {
                arrived.entry((now.id, now.kind)).or_default().push(path);
            }
        }
        let mut ends: Vec<PathBuf> = (self.entries.iter())
            .filter(|(known, entry)| {
                (arrived.get(&(entry.id, entry.kind))).is_some_and(|at| {
                    at.iter().any(|at| {
                        volume_of(at, &scan.new, &self.entries)
                            == volume_of(known, &self.entries, &self.entries)
                    })
                })
            })
            .filter(|(known, _)| !covered(known))
            .map(|(known, _)| known.clone())
            .collect();
        if !ends.is_empty() {
            return (ends, Vec::new(), Vec::new());
        }
        let gone: Vec<(&PathBuf, &Entry)> = (outermost(scan.replaced.clone()).iter())
            .flat_map(|prefix| {
                (self.entries.range(prefix.clone()..))
                    .take_while(|(path, _)| path.starts_with(prefix))
                    .collect::<Vec<_>>()
            })
            .filter(|(path, entry)| {
                !scan.new.get(*path).is_some_and(|now| same(entry, now))
                    && !arrived.contains_key(&(entry.id, entry.kind))
                    && !blocked(path)
                    && volume_of(path, &self.entries, &scan.new).is_none()
            })
            .collect();
        let Some((dir, dev)) = (!gone.is_empty())
            .then(|| open_root(root).ok())
            .flatten()
            .and_then(|dir| Some((stat_fd(dir.as_fd()).ok()?.dev, dir)))
            .map(|(dev, dir)| (dir, dev))
        else {
            return (ends, Vec::new(), Vec::new());
        };
        let linked: HashSet<(FileId, EntryKind)> = (gone.iter())
            .filter(|(_, entry)| entry.linked)
            .map(|(_, entry)| (entry.id, entry.kind))
            .collect();
        let mut links: HashMap<(FileId, EntryKind), Vec<&Path>> = HashMap::new();
        if !linked.is_empty() {
            for (path, entry) in
                (scan.new.iter()).chain((self.entries.iter()).filter(|(path, _)| !covered(path)))
            {
                if linked.contains(&(entry.id, entry.kind)) {
                    links.entry((entry.id, entry.kind)).or_default().push(path);
                }
            }
        }
        let mut hidden = Vec::new();
        let mut unsure = Vec::new();
        let mut moved: Option<&Path> = None;
        for (path, entry) in gone {
            if moved.is_some_and(|folder| path.starts_with(folder)) {
                continue;
            }
            let links = (links.get(&(entry.id, entry.kind))).map_or(&[][..], Vec::as_slice);
            match whereabouts(dir.as_fd(), dev, entry, links) {
                Whereabouts::At(at) if blocked(&at) => {
                    hidden.push(path.clone());
                    moved = Some(path);
                }
                Whereabouts::Hidden => {
                    hidden.push(path.clone());
                    moved = Some(path);
                }
                Whereabouts::At(at) => {
                    if !covered(&at) {
                        ends.push(at);
                    }
                    moved = Some(path);
                }
                Whereabouts::Unsure => {
                    unsure.push(path.clone());
                    moved = Some(path);
                }
                Whereabouts::Nowhere => {
                    ends.extend(
                        (links.iter())
                            .filter(|link| !covered(link))
                            .map(|link| link.to_path_buf()),
                    );
                }
            }
        }
        (ends, hidden, unsure)
    }

    fn scan_scopes(
        &self,
        root: &Path,
        paths: &[PathBuf],
        listed: &BTreeSet<PathBuf>,
        expected: Option<FileId>,
        scan: &mut Scan<'_>,
    ) -> Result<Stat> {
        let root_dir = open_root(root)?;
        let root_seen = now_ns();
        let root_stat = stat_fd(root_dir.as_fd())?;
        if expected.is_some_and(|expected| !root_stat.id.same(&expected)) {
            return Err(Error::RootReplaced);
        }
        (scan.cut)(Cut::RootOpened);

        let mut scopes = Vec::new();
        for path in paths.iter().cloned() {
            if path
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("not a relative path inside the root: {}", quoted(&path)),
                )
                .into());
            }
            if path
                .components()
                .next()
                .is_some_and(|first| first.as_os_str() == PRIVATE_DIR)
            {
                continue;
            }
            scopes.push(self.anchor(path));
        }
        let (listed, unknown): (Vec<&PathBuf>, Vec<&PathBuf>) = (listed.iter()).partition(|dir| {
            (self.entries.get(*dir)).is_some_and(|entry| entry.kind == EntryKind::Dir)
        });
        scopes.extend(unknown.into_iter().map(|dir| self.anchor(dir.clone())));

        let scopes = outermost(scopes);
        if scopes
            .first()
            .is_some_and(|scope| scope.as_os_str().is_empty())
        {
            scan.replaced.push(PathBuf::new());
            (scan.listed).push((
                PathBuf::new(),
                [root_stat.mtime_ns, root_stat.ctime_ns],
                root_seen,
            ));
            scan.dir(root_dir.as_fd(), root_stat.dev, Path::new(""))?;
        } else {
            let mut resolved = Vec::new();
            let mut pending = scopes;
            while let Some(scope) = pending.pop() {
                let parent = scope.parent().unwrap_or(Path::new(""));
                let known = parent.as_os_str().is_empty()
                    || match self.open_known_dir(&root_dir, parent) {
                        Ok(found) => found.is_some(),
                        Err(Error::Io(err)) if denied(&err) => true,
                        Err(err) => return Err(err),
                    };
                if known {
                    resolved.push(scope);
                } else {
                    pending.push(parent.to_path_buf());
                }
            }
            let mut groups: BTreeMap<PathBuf, BTreeSet<OsString>> = BTreeMap::new();
            let resolved = outermost(resolved);
            for scope in &resolved {
                let name = scope
                    .file_name()
                    .expect("scopes are non-empty")
                    .to_os_string();
                let parent = scope.parent().unwrap_or(Path::new("")).to_path_buf();
                groups.entry(parent).or_default().insert(name);
            }
            for dir in listed {
                if !resolved.iter().any(|scope| dir.starts_with(scope)) {
                    groups.entry(dir.clone()).or_default();
                }
            }
            for (parent, focus) in groups {
                let result = self.scan_children(scan, &root_dir, root_stat.dev, &parent, &focus);
                scan.unless_denied(&parent, result)?;
            }
        }
        Ok(root_stat)
    }

    fn publish(
        &mut self,
        root: &Path,
        root_stat: &Stat,
        scan: &mut Scan<'_>,
        hidden: Vec<PathBuf>,
        unsure: Vec<PathBuf>,
        keep: Keep<'_>,
    ) -> Result<Rescan> {
        let again = open_root(root)?;
        if !stat_fd(again.as_fd())?.is_at(root_stat.dev, &root_stat.id) {
            return Err(Error::RootReplaced);
        }
        revalidate(again.as_fd(), std::mem::take(&mut scan.bindings))?;

        let mut old = BTreeMap::new();
        for prefix in &scan.replaced {
            old.extend(
                self.entries
                    .range(prefix.clone()..)
                    .take_while(|(path, _)| path.starts_with(prefix))
                    .map(|(path, entry)| (path.clone(), *entry)),
            );
        }
        let mut was_left_out = Vec::new();
        let mut unseen_left_out = BTreeSet::new();
        for prefix in &scan.replaced {
            for (path, entry) in (self.left_out.range(prefix.clone()..))
                .take_while(|(path, _)| path.starts_with(prefix))
            {
                was_left_out.push(path.clone());
                if !scan.new.contains_key(path) {
                    old.insert(path.clone(), *entry);
                    unseen_left_out.insert(path.clone());
                }
            }
        }
        let (left_out, mut still_left_out) = take_left_out(&mut old, &scan.left_out);
        let mount_names: BTreeSet<OsString> = (scan.new.iter())
            .filter(|(_, now)| now.mount.is_some())
            .filter_map(|(path, _)| path.file_name().map(OsStr::to_os_string))
            .collect();
        let mut held_from = BTreeMap::new();
        for (path, known) in &old {
            let held = scan.mounts.contains_key(path);
            if held
                && (scan.new.contains_key(path)
                    || (scan.unreadable.iter()).any(|blocked| path.starts_with(blocked)))
            {
                continue;
            }
            let hides_children = known.kind == EntryKind::Dir
                && known.mount.is_none()
                && path
                    .file_name()
                    .is_some_and(|name| mount_names.contains(name))
                && (old.range(path.clone()..).nth(1))
                    .is_some_and(|(inside, _)| inside.starts_with(path));
            if !held && !hides_children {
                continue;
            }
            let Some(at) = moved_to(path, &old, &scan.new, &self.entries) else {
                continue;
            };
            let now = scan.new.get(&at);
            let keep = match known.mount {
                Some(_) => {
                    !now.is_some_and(|now| known.is_back(now, scan.plain_ids.get(&at).copied()))
                }
                None => now.is_some_and(|now| {
                    now.mount.is_some()
                        && (held
                            || !now.id.same(&known.id)
                            || now.mount == Some(Mount::Unidentified))
                }),
            };
            if !keep {
                continue;
            }
            scan.new.insert(at.clone(), *known);
            scan.detached.push((at.clone(), known.detached()));
            scan.unreadable.push(at.clone());
            held_from.insert(at, path.clone());
        }
        let unreadable = std::mem::take(&mut scan.unreadable);
        let mut arrivals: HashMap<_, Vec<_>> = HashMap::new();
        if !unreadable.is_empty() {
            for (path, now) in &scan.new {
                let observed = !(unreadable.iter()).any(|blocked| {
                    path.starts_with(blocked) && (path != blocked || held_from.contains_key(path))
                });
                if observed && (now.kind == EntryKind::Dir || !now.linked) {
                    let volume = volume_of(path, &scan.new, &self.entries).map(Path::to_path_buf);
                    (arrivals.entry((now.id, now.kind)).or_default()).push((path.clone(), volume));
                }
            }
        }
        let arrived = |entry: &Entry, volume: &Option<PathBuf>| {
            (arrivals.get(&(entry.id, entry.kind)))
                .into_iter()
                .flatten()
                .find(|(to, at)| {
                    at == volume
                        && (stat_rel(again.as_fd(), to).ok())
                            .and_then(|stat| stat.entry())
                            .is_some_and(|now| {
                                now.id.same(&entry.id) && now.kind == entry.kind && !now.linked
                            })
                })
                .map(|(to, _)| to)
        };
        let elsewhere = |path: &Path,
                         entry: &Entry,
                         volume: Option<PathBuf>,
                         tree: &BTreeMap<PathBuf, Entry>,
                         new: &BTreeMap<PathBuf, Entry>| {
            let Some(to) = arrived(entry, &volume) else {
                return false;
            };
            (tree.range(path.to_path_buf()..))
                .skip(1)
                .take_while(|(inside, _)| inside.starts_with(path))
                .all(|(inside, before)| {
                    let at = to.join(inside.strip_prefix(path).expect("taken below the folder"));
                    (new.get(&at).or_else(|| scan.left_out.get(&at)))
                        .is_some_and(|now| now.id.same(&before.id) && now.kind == before.kind)
                        || (unreadable.iter())
                            .any(|blocked| at.starts_with(blocked) && at != *blocked)
                        || arrived(before, &volume).is_some()
                })
        };
        let mut departed = BTreeSet::new();
        for blocked in &unreadable {
            let unscanned: Vec<(PathBuf, Entry)> = (self.entries.range(blocked.clone()..))
                .take_while(|(path, _)| path.starts_with(blocked))
                .filter(|(path, _)| *path != blocked && !old.contains_key(*path))
                .map(|(path, entry)| (path.clone(), *entry))
                .collect();
            for (path, entry) in unscanned {
                if path.ancestors().any(|above| departed.contains(above)) {
                    old.insert(path, entry);
                } else if elsewhere(
                    &path,
                    &entry,
                    volume_now(&path, &self.entries, &scan.new, &self.entries),
                    &self.entries,
                    &scan.new,
                ) {
                    old.insert(path.clone(), entry);
                    departed.insert(path);
                }
            }
        }
        for blocked in &unreadable {
            scan.new
                .retain(|path, _| path == blocked || !path.starts_with(blocked));
            let now = scan.new.get(blocked);
            let known = match held_from.get(blocked) {
                Some(from) => from.clone(),
                None if (old.get(blocked)).is_some_and(|before| Some(before) == now) => {
                    blocked.clone()
                }
                None => now
                    .and_then(|now| {
                        let volume = volume_of(blocked, &scan.new, &self.entries);
                        old.iter().find(|(path, before)| {
                            before.kind == EntryKind::Dir
                                && before.id.same(&now.id)
                                && volume_now(path, &old, &scan.new, &self.entries).as_deref()
                                    == volume
                        })
                    })
                    .map_or(blocked, |(path, _)| path)
                    .clone(),
            };
            let below: Vec<(PathBuf, Entry)> = (old.range(known.clone()..))
                .take_while(|(path, _)| path.starts_with(&known))
                .filter(|(path, _)| **path != known)
                .map(|(path, entry)| (path.clone(), *entry))
                .collect();
            for (path, entry) in below {
                if (path.ancestors())
                    .take_while(|above| *above != known && above.starts_with(&known))
                    .any(|above| departed.contains(above))
                {
                    continue;
                }
                if elsewhere(
                    &path,
                    &entry,
                    volume_now(&path, &old, &scan.new, &self.entries),
                    &old,
                    &scan.new,
                ) {
                    departed.insert(path);
                    continue;
                }
                let at = blocked.join(path.strip_prefix(&known).expect("taken below the folder"));
                if unseen_left_out.remove(&path) {
                    old.remove(&path);
                    still_left_out.push((at, entry));
                } else {
                    scan.new.insert(at, entry);
                }
            }
        }
        let mut unsettled = keep_unconfirmed(
            root,
            root_stat.dev,
            &root_stat.id,
            again.as_fd(),
            &old,
            &mut scan.new,
            &departed,
            &mut *scan.cut,
        )?;
        let on_disk = |at: &Path, id: &FileId| {
            (stat_rel(again.as_fd(), at).ok())
                .and_then(|stat| stat.entry())
                .is_some_and(|now| now.id.same(id))
        };
        let left_out_ids: HashSet<FileId> = (scan.left_out.values())
            .map(|entry| entry.id.without_handle())
            .collect();
        let (twice, vanished) = placements(
            &scan.new,
            &old,
            &left_out_ids,
            &unreadable,
            &held_from,
            &self.entries,
        );
        let mut fresh: Vec<PathBuf> = (twice.into_iter())
            .filter(|(first, second)| {
                let id = scan.new[first].id;
                !(on_disk(first, &id) && on_disk(second, &id))
            })
            .flat_map(|(first, second)| [first, second])
            .collect();
        if !vanished.is_empty() {
            let (found, unsure) = missed_moves(
                again.as_fd(),
                std::mem::take(&mut scan.listed),
                &vanished,
                &|path, entry| {
                    (scan.new.get(path))
                        .or_else(|| scan.left_out.get(path))
                        .or_else(|| self.entries.get(path))
                        .is_some_and(|known| known.id.same(&entry.id))
                },
                &|path| volume_of(path, &scan.new, &self.entries).map(Path::to_path_buf),
                &mut scan.ticks,
                &mut *scan.cut,
            );
            if unsure || found.iter().any(|(_, key)| key.is_none()) {
                fresh.extend(vanished.values().flatten().map(|(path, _)| path.clone()));
            }
            for (at, key) in found {
                fresh.extend(
                    (key.iter())
                        .flat_map(|key| &vanished[key])
                        .map(|(path, _)| path.clone()),
                );
                fresh.push(at);
            }
        }
        let gone: HashSet<PathBuf> = (vanished.values().flatten())
            .map(|(path, _)| path.clone())
            .collect();
        let mut straddled = BTreeSet::new();
        if !fresh.is_empty() {
            let (moved, _, left) = split(&scan.new, &old);
            let mut placed: HashMap<_, HashMap<PathBuf, FileId>> = HashMap::new();
            let mut placed_at = HashMap::new();
            for (path, entry) in moved {
                let volume = volume_of(path, &scan.new, &self.entries).map(Path::to_path_buf);
                let key = (volume, entry.id.without_handle());
                (placed.entry(key.clone()).or_default()).insert(path.clone(), entry.id);
                placed_at.insert(path.clone(), key);
            }
            let mut sources: HashMap<_, Vec<(PathBuf, FileId)>> = HashMap::new();
            let (mut pushed, mut verified, mut drained) =
                (HashSet::new(), HashSet::new(), HashSet::new());
            for (path, entry) in left {
                let volume = volume_now(path, &old, &scan.new, &self.entries);
                (sources
                    .entry((volume, entry.id.without_handle()))
                    .or_default())
                .push((path.clone(), entry.id));
            }
            while !fresh.is_empty() {
                fresh.sort();
                fresh.dedup_by(|inner, outer| inner.starts_with(&*outer));
                straddled.extend(fresh.iter().cloned());
                let mut next = Vec::new();
                for path in &fresh {
                    let below: Vec<(PathBuf, Entry)> = (scan.new.range(path.clone()..))
                        .take_while(|(inside, _)| inside.starts_with(path))
                        .map(|(inside, entry)| (inside.clone(), *entry))
                        .collect();
                    for (inside, entry) in below {
                        scan.new.remove(&inside);
                        let Some(key) = placed_at.remove(&inside) else {
                            continue;
                        };
                        let others = placed.get_mut(&key).expect("indexed at its path");
                        others.remove(&inside);
                        if others.values().any(|id| id.same(&entry.id))
                            || left_out_ids.contains(&key.1)
                        {
                            continue;
                        }
                        next.extend(
                            (sources.get(&key).into_iter().flatten())
                                .filter(|(at, id)| id.same(&entry.id) && !gone.contains(at))
                                .map(|(at, _)| at.clone()),
                        );
                    }
                    let restored: Vec<(PathBuf, Entry)> = (old.range(path.clone()..))
                        .take_while(|(inside, _)| inside.starts_with(path))
                        .map(|(inside, entry)| (inside.clone(), *entry))
                        .collect();
                    scan.new.extend(restored.iter().cloned());
                    for (inside, entry) in restored {
                        let volume =
                            volume_of(&inside, &scan.new, &self.entries).map(Path::to_path_buf);
                        let met = (volume.clone(), entry.id);
                        if !observed(&inside, &unreadable, &held_from) || drained.contains(&met) {
                            continue;
                        }
                        let here = on_disk(&inside, &entry.id);
                        if here && verified.contains(&met) {
                            continue;
                        }
                        for (at, id) in (placed.get(&(volume, entry.id.without_handle())))
                            .into_iter()
                            .flatten()
                        {
                            if id.same(&entry.id)
                                && observed(at, &unreadable, &held_from)
                                && !pushed.contains(at)
                                && !(here && on_disk(at, id))
                            {
                                pushed.insert(at.clone());
                                next.push(at.clone());
                            }
                        }
                        match here {
                            true => verified.insert(met),
                            false => drained.insert(met),
                        };
                    }
                }
                next.retain(|path| !path.ancestors().any(|above| straddled.contains(above)));
                fresh = next;
            }
        }
        unsettled.extend(straddled);
        let arrived: HashSet<(FileId, EntryKind)> = (scan.new.iter())
            .filter(|(path, now)| {
                !(old.get(*path))
                    .is_some_and(|before| before.id.same(&now.id) && before.kind == now.kind)
            })
            .map(|(_, now)| (now.id, now.kind))
            .collect();
        for kept in &unsettled {
            for path in
                (unseen_left_out.range(kept.clone()..)).take_while(|path| path.starts_with(kept))
            {
                if let Some(entry) = scan.new.remove(path) {
                    old.remove(path);
                    still_left_out.push((path.clone(), entry));
                }
            }
        }
        let stays = match &scan.rules {
            Some(rules) => {
                let (held, left_out): (BTreeMap<PathBuf, Entry>, BTreeMap<PathBuf, Entry>) =
                    (rules.held_in(&scan.replaced).into_iter()).partition(|(path, entry)| {
                        (path
                            .ancestors()
                            .take_while(|above| !above.as_os_str().is_empty()))
                        .all(|above| {
                            let kind = if above == path {
                                entry.kind
                            } else {
                                EntryKind::Dir
                            };
                            rules.status(above, kind) == Ignored::No
                        })
                    });
                let mut there: BTreeMap<PathBuf, Entry> = (held.keys())
                    .filter_map(|path| Some((path.clone(), *scan.new.get(path)?)))
                    .collect();
                let mut stays = keep_unconfirmed(
                    root,
                    root_stat.dev,
                    &root_stat.id,
                    again.as_fd(),
                    &held,
                    &mut there,
                    &BTreeSet::new(),
                    &mut *scan.cut,
                )?;
                stays.extend(left_out.into_keys().filter(|path| {
                    let parent = path.parent().unwrap_or(Path::new(""));
                    observe(again.as_fd(), parent, &mut *scan.cut).is_none()
                }));
                stays
            }
            None => Vec::new(),
        };
        let mut settled: Vec<PathBuf> = (scan.new.iter())
            .filter(|(path, now)| {
                now.kind == EntryKind::File
                    && match (old.get(*path).map(|before| before.ctime), now.ctime) {
                        (_, Ctime::Racy { .. }) => false,
                        (
                            Some(Ctime::Racy { at, mode, .. }),
                            Ctime::Settled {
                                at: now_at,
                                mode: now_mode,
                            },
                        ) => (at, mode) != (now_at, now_mode),
                        (before, now) => before != Some(now),
                    }
            })
            .map(|(path, _)| path.clone())
            .collect();
        scan.marks.carry(&mut old, &scan.new);
        let mut changes = diff(&old, &scan.new, &self.entries);
        if let Some(rules) = &mut scan.rules {
            rules.settle();
            let unread: Vec<PathBuf> = (unreadable.iter().chain(&unsettled).chain(&stays))
                .cloned()
                .collect();
            let earlier = rules.cover(&scan.replaced, &unread);
            let held = rules.take_held(&scan.replaced, &stays);
            let (arrivals, postponed) = find_ignored(
                &mut changes,
                &old,
                &scan.new,
                &self.entries,
                held,
                &earlier,
                rules,
                again.as_fd(),
                &mut *scan.cut,
            );
            let arrivals: BTreeSet<PathBuf> = arrivals.into_iter().collect();
            scan.new
                .retain(|path, _| !path.ancestors().any(|above| arrivals.contains(above)));
            rules.postpone(&scan.replaced, postponed.keys().cloned());
            scan.new.extend(postponed);
            unsettled.extend(outermost(
                rules.held().chain(rules.postponed()).cloned().collect(),
            ));
        }
        let whole = (scan.replaced.iter()).any(|prefix| prefix.as_os_str().is_empty());
        let doubtful = match keep {
            Keep::Unexplained { .. } if !whole => old.keys().cloned().collect(),
            _ => [unsure, hidden.clone()].concat(),
        };
        let hidden: BTreeSet<PathBuf> = hidden.into_iter().collect();
        let mut removed = BTreeSet::new();
        let mut created = BTreeSet::new();
        let mut moved_from = BTreeSet::new();
        let mut left_from = BTreeSet::new();
        let mut moves = Vec::new();
        for change in &changes {
            match change {
                Change::Removed { path, .. } | Change::Missing { path, .. } => {
                    removed.insert(path.clone());
                }
                Change::Created { path, .. } => {
                    created.insert(path.clone());
                }
                Change::Renamed { from, to, .. } => {
                    moved_from.insert(from.clone());
                    moves.push((from.clone(), to.clone()));
                }
                Change::Ignored { from, .. } => {
                    moved_from.insert(from.clone());
                    left_from.insert(from.clone());
                }
                _ => {}
            }
        }
        let moves: Vec<(&Path, &Path)> = (moves.iter())
            .map(|(from, to)| (from.as_path(), to.as_path()))
            .collect();
        let settling: BTreeSet<PathBuf> = unsettled.iter().cloned().collect();
        let mut unplaced = BTreeSet::new();
        let mut still_hidden = BTreeSet::new();
        let mut holding = BTreeMap::new();
        for path in doubtful {
            let Some(gone) = path.ancestors().find(|above| removed.contains(*above)) else {
                continue;
            };
            if gone.ancestors().any(|above| left_from.contains(above)) {
                continue;
            }
            let place = carried(gone, &moves);
            let occupant: Vec<PathBuf> = (scan.new.range(place.clone()..))
                .map(|(inside, _)| inside)
                .take_while(|inside| inside.starts_with(&place))
                .cloned()
                .collect();
            let proven = hidden.contains(&path);
            if (proven && !occupant.is_empty())
                || moves.iter().any(|(_, to)| to.starts_with(&place))
                || (occupant.iter())
                    .any(|inside| !inside.ancestors().any(|above| created.contains(above)))
            {
                continue;
            }
            if place.ancestors().any(|above| {
                (!proven
                    && self.unplaced.contains(above)
                    && !matches!(keep, Keep::Unexplained { waiting } if waiting.contains(above)))
                    || settling.contains(above)
                    || unplaced.contains(above)
                    || still_hidden.contains(above)
            }) {
                continue;
            }
            let kept: Vec<(PathBuf, Entry)> = (old.range(gone.to_path_buf()..))
                .take_while(|(inside, _)| inside.starts_with(gone))
                .filter(|(inside, entry)| {
                    !arrived.contains(&(entry.id, entry.kind))
                        && !(inside.ancestors())
                            .take_while(|above| above.starts_with(gone))
                            .any(|above| moved_from.contains(above))
                })
                .map(|(inside, entry)| (carried(inside, &moves), *entry))
                .collect();
            if kept.is_empty() {
                continue;
            }
            for inside in occupant {
                scan.new.remove(&inside);
            }
            scan.new.extend(kept);
            holding.insert(gone.to_path_buf(), place.clone());
            match proven {
                true => still_hidden.insert(place),
                false => unplaced.insert(place),
            };
        }
        for (gone, place) in &holding {
            for path in
                (unseen_left_out.range(gone.clone()..)).take_while(|path| path.starts_with(gone))
            {
                let at = place.join(path.strip_prefix(gone).expect("taken below the held path"));
                if let Some(entry) = scan.new.remove(&at) {
                    old.remove(path);
                    still_left_out.push((at, entry));
                }
            }
        }
        changes.retain(|change| match change {
            Change::Removed { path, .. } | Change::Missing { path, .. } => {
                !holding.contains_key(path)
            }
            Change::Created { path, .. } => {
                !(holding.values()).any(|place| path.starts_with(place))
            }
            _ => true,
        });
        unsettled.extend(unplaced.iter().cloned());
        unsettled.extend(still_hidden);
        unsettled.sort();
        unsettled.dedup();
        for path in old.keys().chain(&left_out) {
            self.entries.remove(path);
        }
        self.entries.append(&mut scan.new);
        self.detached
            .retain(|path, _| !scan.replaced.iter().any(|prefix| path.starts_with(prefix)));
        self.detached.extend(scan.detached.drain(..));
        self.unplaced.retain(|path| {
            !scan.replaced.iter().any(|prefix| path.starts_with(prefix))
                || unreadable.iter().any(|blocked| path.starts_with(blocked))
        });
        self.unplaced.extend(unplaced);
        for path in &was_left_out {
            self.left_out.remove(path);
        }
        self.left_out.extend(still_left_out);
        self.left_out.extend(changes.iter().filter_map(|change| {
            match change {
                Change::Ignored { to, kind, id, .. } => (scan.left_out.get(to))
                    .filter(|now| now.kind == *kind && now.id.same(id))
                    .map(|now| (to.clone(), *now)),
                _ => None,
            }
        }));
        let (rules, read) = match scan.rules.take() {
            Some(rules) => rules.finish(|dir| {
                self.entries
                    .get(dir)
                    .is_some_and(|entry| entry.kind == EntryKind::Dir)
            }),
            None => (Vec::new(), Vec::new()),
        };
        let reported: HashSet<&Path> = (changes.iter())
            .filter_map(|change| match change {
                Change::Created { path, .. } | Change::Modified { path } => Some(path.as_path()),
                _ => None,
            })
            .collect();
        settled.retain(|path| !reported.contains(path.as_path()));
        Ok(Rescan {
            changes,
            unsettled,
            unreadable,
            root: root_stat.id,
            rules,
            read,
            settled,
        })
    }

    /// Rescans the children of the known folder `parent`: every name in
    /// `focus`, every listed name the snapshot does not know, and every known
    /// child the listing no longer contains. When a rules file of the folder
    /// changed, it rescans every child, because the change can include or
    /// leave out anything below the folder.
    fn scan_children(
        &self,
        scan: &mut Scan<'_>,
        root: &OwnedFd,
        root_dev: u64,
        parent: &Path,
        focus: &BTreeSet<OsString>,
    ) -> Result<()> {
        let (dir, dev) = if parent.as_os_str().is_empty() {
            (root.try_clone()?, root_dev)
        } else {
            let (dir, chain) = self.open_known_dir(root, parent)?.ok_or(Error::Changed)?;
            let dev = chain.last().map_or(root_dev, |(_, dev, _)| *dev);
            scan.bindings.extend(chain);
            (dir, dev)
        };
        let dir = dir.as_fd();
        let seen = now_ns();
        let stat = stat_fd(dir)?;
        (scan.listed).push((parent.to_path_buf(), [stat.mtime_ns, stat.ctime_ns], seen));
        let mut names = list(dir)?;
        let widen = match scan.read_rules(dir, parent, &names) {
            RulesRead::Unknown => {
                scan.unreadable.push(parent.to_path_buf());
                return Ok(());
            }
            read => read == RulesRead::Changed,
        };
        names.retain(|name| !private(parent, name));
        for name in &names {
            let child = parent.join(name);
            if widen || focus.contains(name) || !self.entries.contains_key(&child) {
                scan.replaced.push(child.clone());
                scan.entry(dir, dev, name, child)?;
            }
        }
        for name in focus.difference(&names) {
            scan.replaced.push(parent.join(name));
        }
        let known = |entries: &BTreeMap<PathBuf, Entry>| {
            (entries.range(parent.to_path_buf()..))
                .take_while(|(path, _)| path.starts_with(parent))
                .filter(|(path, _)| path.parent() == Some(parent))
                .filter(|(path, _)| !path.file_name().is_some_and(|name| names.contains(name)))
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>()
        };
        scan.replaced.extend(known(&self.entries));
        scan.replaced.extend(known(&self.left_out));
        Ok(())
    }

    /// Opens the folder the snapshot knows at `rel`, one component at a time,
    /// each one listed under its exact name and opened without following links.
    /// Returns the folder with every folder opened on the way and its identity,
    /// or `None` if that path no longer leads to the same folder.
    #[allow(clippy::type_complexity)]
    fn open_known_dir(
        &self,
        root: &OwnedFd,
        rel: &Path,
    ) -> Result<Option<(OwnedFd, Vec<(PathBuf, u64, FileId)>)>> {
        let Some(known) = self.entries.get(rel).filter(|e| e.kind == EntryKind::Dir) else {
            return Ok(None);
        };
        let mut dir = root.try_clone()?;
        let mut dev = stat_fd(dir.as_fd())?.dev;
        let mut path = PathBuf::new();
        let mut chain = Vec::new();
        for component in rel.components() {
            if !list(dir.as_fd())?.contains(component.as_os_str()) {
                return Ok(None);
            }
            dir = match open_dir_at(dir.as_fd(), component.as_os_str()) {
                Ok(next) => next,
                Err(err) if is_gone(&err) => return Ok(None),
                Err(err) => return Err(err.into()),
            };
            path.push(component);
            let stat = stat_fd(dir.as_fd())?;
            let above = self.entries.get(&path);
            if stat.dev != dev || above.is_some_and(|above| above.mount.is_some()) {
                let Some(above) = above else {
                    return Ok(None);
                };
                let found = (stat.dev != dev).then(|| mount_at(dir.as_fd()).0);
                let unread = above.mount == Some(Mount::Unidentified);
                if unread || above.mount != found || !above.id.same(&stat.id) {
                    return Ok(None);
                }
            }
            dev = stat.dev;
            chain.push((path.clone(), stat.dev, stat.id));
        }
        let same = chain.last().is_some_and(|(_, _, id)| id.same(&known.id));
        Ok(same.then_some((dir, chain)))
    }

    fn anchor(&self, mut path: PathBuf) -> PathBuf {
        while let Some(parent) = path.parent() {
            let known_dir = self
                .entries
                .get(parent)
                .is_some_and(|entry| entry.kind == EntryKind::Dir);
            if parent.as_os_str().is_empty() || known_dir {
                break;
            }
            path = parent.to_path_buf();
        }
        path
    }
}

fn moved_to(
    path: &Path,
    old: &BTreeMap<PathBuf, Entry>,
    new: &BTreeMap<PathBuf, Entry>,
    beyond: &BTreeMap<PathBuf, Entry>,
) -> Option<PathBuf> {
    let parent = path.parent()?;
    let Some(before) = old.get(parent) else {
        return Some(path.to_path_buf());
    };
    if new.get(parent).is_some_and(|now| now.id.same(&before.id)) {
        return Some(path.to_path_buf());
    }
    let volume = volume_now(parent, old, new, beyond);
    match (new.iter()).find(|(at, now)| {
        now.kind == EntryKind::Dir
            && now.id.same(&before.id)
            && volume_of(at, new, beyond) == volume.as_deref()
    }) {
        Some((at, _)) => Some(at.join(path.file_name()?)),
        None => (new.get(parent))
            .is_some_and(|now| now.kind == EntryKind::Dir)
            .then(|| path.to_path_buf()),
    }
}

type Vanishing = (Option<PathBuf>, FileId, EntryKind);

type Vanished = HashMap<Vanishing, Vec<(PathBuf, FileId)>>;

type Placed<'a> = Vec<(&'a PathBuf, &'a Entry)>;

fn split<'n, 'o>(
    new: &'n BTreeMap<PathBuf, Entry>,
    old: &'o BTreeMap<PathBuf, Entry>,
) -> (Placed<'n>, Placed<'n>, Placed<'o>) {
    let (mut moved, mut kept, mut gone) = (Vec::new(), Vec::new(), Vec::new());
    let mut before = old.iter().peekable();
    for (path, entry) in new {
        gone.extend(std::iter::from_fn(|| {
            before.next_if(|(was, _)| *was < path)
        }));
        match before.next_if(|(was, _)| *was == path) {
            Some((_, was)) if was.id.same(&entry.id) => kept.push((path, entry)),
            other => {
                gone.extend(other);
                moved.push((path, entry));
            }
        }
    }
    gone.extend(before);
    (moved, kept, gone)
}

fn observed(path: &Path, unreadable: &[PathBuf], held: &BTreeMap<PathBuf, PathBuf>) -> bool {
    !(unreadable.iter())
        .any(|blocked| path.starts_with(blocked) && (path != blocked || held.contains_key(path)))
}

fn placements<'n>(
    new: &'n BTreeMap<PathBuf, Entry>,
    old: &BTreeMap<PathBuf, Entry>,
    left_out: &HashSet<FileId>,
    unreadable: &[PathBuf],
    held: &BTreeMap<PathBuf, PathBuf>,
    entries: &BTreeMap<PathBuf, Entry>,
) -> (Vec<(PathBuf, PathBuf)>, Vanished) {
    let (moved, kept, gone) = split(new, old);
    let key =
        |at: &'n Path, entry: &Entry| (volume_of(at, new, entries), entry.id.without_handle());
    let mut first = HashMap::new();
    let mut twice = Vec::new();
    let mut meet = |path: &'n PathBuf, entry: &Entry, other: &'n PathBuf, was: &Entry| {
        if !observed(path, unreadable, held)
            || !observed(other, unreadable, held)
            || !entry.id.same(&was.id)
        {
            return;
        }
        let first = *first.entry(key(path, entry)).or_insert(other);
        if first != path {
            twice.push((first.clone(), path.clone()));
        }
    };
    let mut placed: HashMap<_, (&PathBuf, &Entry)> = HashMap::with_capacity(moved.len());
    for (path, entry) in moved {
        if let Some((other, was)) = placed.insert(key(path, entry), (path, entry)) {
            meet(path, entry, other, was);
        }
    }
    if !placed.is_empty() {
        for (path, entry) in kept {
            if let Some(&(other, was)) = placed.get(&key(path, entry)) {
                meet(path, entry, other, was);
            }
        }
    }
    let mut vanished = Vanished::new();
    for (path, entry) in gone {
        let volume = volume_now(path, old, new, entries);
        if (placed.get(&(volume.as_deref(), entry.id.without_handle())))
            .is_some_and(|(_, now)| now.id.same(&entry.id))
            || left_out.contains(&entry.id.without_handle())
        {
            continue;
        }
        (vanished
            .entry((volume, entry.id.without_handle(), entry.kind))
            .or_default())
        .push((path.clone(), entry.id));
    }
    (twice, vanished)
}

fn missed_moves(
    root: BorrowedFd<'_>,
    listed: Vec<(PathBuf, [i128; 2], i128)>,
    vanished: &Vanished,
    known: &dyn Fn(&Path, &Entry) -> bool,
    volume: &dyn Fn(&Path) -> Option<PathBuf>,
    ticks: &mut HashMap<u64, i128>,
    cut: &mut dyn FnMut(Cut<'_>),
) -> (Vec<(PathBuf, Option<Vanishing>)>, bool) {
    let mut found = Vec::new();
    let mut unsure = false;
    for (rel, times, seen) in listed {
        let Ok(Some((dir, _))) = open_attached(root, &rel, cut) else {
            unsure = true;
            continue;
        };
        let Ok(now) = stat_fd(dir.as_fd()) else {
            unsure = true;
            continue;
        };
        let tick = *ticks
            .entry(now.dev)
            .or_insert_with(|| timestamp_tick(dir.as_fd()));
        if [now.mtime_ns, now.ctime_ns] == times
            && (times.iter()).all(|at| seen >= at + tick_of(*at, Some(tick)))
        {
            continue;
        }
        let Ok(names) = list(dir.as_fd()) else {
            unsure = true;
            continue;
        };
        for name in names.iter().filter(|name| !private(&rel, name)) {
            let entry = match stat_at(dir.as_fd(), name) {
                Ok(stat) => stat.entry(),
                Err(err) if is_gone(&err) => None,
                Err(_) => {
                    unsure = true;
                    None
                }
            };
            let Some(entry) = entry else {
                continue;
            };
            let at = rel.join(name);
            if known(&at, &entry) {
                continue;
            }
            let key = (volume(&at), entry.id.without_handle(), entry.kind);
            if (vanished.get(&key))
                .is_some_and(|gone| gone.iter().any(|(_, id)| id.same(&entry.id)))
            {
                found.push((at, Some(key)));
            } else if entry.kind == EntryKind::Dir {
                found.push((at, None));
            }
        }
    }
    (found, unsure)
}

/// Checks that every folder the scan opened is still the folder at its path,
/// opening each again from the root without following links. A folder that
/// was moved, replaced or made unreadable after the scan opened it means the
/// scan may have read a detached copy, so nothing it saw is published.
fn revalidate(root: BorrowedFd<'_>, mut bindings: Vec<(PathBuf, u64, FileId)>) -> Result<()> {
    bindings.sort_by(|a, b| a.0.cmp(&b.0));
    bindings.dedup_by(|a, b| a.0 == b.0);
    let mut open: Vec<(PathBuf, OwnedFd)> = Vec::new();
    for (path, dev, id) in bindings {
        while open.last().is_some_and(|(dir, _)| !path.starts_with(dir)) {
            open.pop();
        }
        let (parent_path, parent) = match open.last() {
            Some((dir, fd)) => (dir.as_path(), fd.as_fd()),
            None => (Path::new(""), root),
        };
        let (Some(name), true) = (path.file_name(), path.parent() == Some(parent_path)) else {
            return Err(Error::Changed);
        };
        let dir = open_dir_at(parent, name).map_err(|_| Error::Changed)?;
        if !stat_fd(dir.as_fd()).is_ok_and(|stat| stat.is_at(dev, &id)) {
            return Err(Error::Changed);
        }
        open.push((path, dir));
    }
    Ok(())
}

/// Keeps the old entries of every path the scan saw disappear, and of
/// everything below it, unless it is confirmed gone, and returns the paths
/// kept. A path is confirmed gone when its parent, looked at from the root
/// through `attached`, does not hold its exact name with the old kind, and a
/// final look from a freshly opened root sees that parent unchanged, or when
/// it is below a path in `departed`, whose identity the scan found elsewhere.
#[allow(clippy::too_many_arguments)]
fn keep_unconfirmed(
    root: &Path,
    root_dev: u64,
    root_id: &FileId,
    attached: BorrowedFd<'_>,
    old: &BTreeMap<PathBuf, Entry>,
    new: &mut BTreeMap<PathBuf, Entry>,
    departed: &BTreeSet<PathBuf>,
    cut: &mut dyn FnMut(Cut<'_>),
) -> Result<Vec<PathBuf>> {
    let gone: Vec<(PathBuf, EntryKind)> = old
        .iter()
        .filter(|(path, before)| {
            !new.get(*path)
                .is_some_and(|after| after.kind == before.kind)
                && !path.ancestors().any(|above| departed.contains(above))
        })
        .map(|(path, before)| (path.clone(), before.kind))
        .collect();
    let mut parents: BTreeMap<PathBuf, Option<Seen>> = BTreeMap::new();
    let mut confirmed = Vec::new();
    let mut kept = Vec::new();
    let mut checked: Option<PathBuf> = None;
    for (path, kind) in gone {
        if checked.as_ref().is_some_and(|done| path.starts_with(done)) {
            continue;
        }
        checked = Some(path.clone());
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            kept.push(path);
            continue;
        };
        let seen = parents
            .entry(parent.to_path_buf())
            .or_insert_with(|| observe(attached, parent, cut));
        let gone_now = match seen {
            None => false,
            Some(Seen::Missing) => true,
            Some(Seen::Listed { names, .. }) if !names.contains(name) => true,
            Some(Seen::Listed { .. }) => !may_hold(attached, parent, name, kind, cut),
        };
        if gone_now {
            confirmed.push(path);
        } else {
            kept.push(path);
        }
    }

    let fresh = open_root(root)?;
    if !stat_fd(fresh.as_fd())?.is_at(root_dev, root_id) {
        return Err(Error::RootReplaced);
    }
    let mut stable: HashMap<PathBuf, bool> = HashMap::new();
    for path in confirmed {
        let parent = path.parent().expect("confirmed paths have a parent");
        let unchanged = *stable
            .entry(parent.to_path_buf())
            .or_insert_with(|| parents.get(parent) == Some(&observe(fresh.as_fd(), parent, cut)));
        if !unchanged {
            kept.push(path);
        }
    }

    kept.sort();
    for path in &kept {
        new.retain(|inside, _| !inside.starts_with(path));
        new.extend(
            old.range(path.clone()..)
                .take_while(|(inside, _)| inside.starts_with(path))
                .map(|(inside, entry)| (inside.clone(), *entry)),
        );
    }
    Ok(kept)
}

enum Whereabouts {
    At(PathBuf),
    Hidden,
    Unsure,
    Nowhere,
}

fn whereabouts(root: BorrowedFd<'_>, root_dev: u64, entry: &Entry, links: &[&Path]) -> Whereabouts {
    let is_it = |stat: &Stat| {
        stat.dev == root_dev
            && (stat.entry()).is_some_and(|now| now.id.same(&entry.id) && now.kind == entry.kind)
    };
    let rel = match located(root, entry) {
        Ok(Some(rel)) => rel,
        Ok(None) => return Whereabouts::Nowhere,
        Err(err) if denied(&err) => return Whereabouts::Hidden,
        Err(_) => return Whereabouts::Nowhere,
    };
    if private(Path::new(""), rel.iter().next().unwrap_or_default()) {
        return Whereabouts::Nowhere;
    }
    match stat_rel(root, &rel) {
        Ok(stat) if is_it(&stat) && links.contains(&rel.as_path()) => Whereabouts::Nowhere,
        Ok(stat) if is_it(&stat) => Whereabouts::At(rel),
        Err(err) if denied(&err) => Whereabouts::Hidden,
        Ok(_) | Err(_) if !linked_anywhere(root_dev, entry) => Whereabouts::Nowhere,
        Ok(_) | Err(_) => Whereabouts::Unsure,
    }
}

#[cfg(target_os = "macos")]
fn linked_anywhere(dev: u64, entry: &Entry) -> bool {
    let by_id = PathBuf::from(format!("/.vol/{dev}/{}", entry.id.ino));
    match rustix::fs::lstat(&by_id) {
        Ok(stat) => (from_stat(stat).entry())
            .is_some_and(|now| now.id.same(&entry.id) && now.kind == entry.kind),
        Err(err) => err != rustix::io::Errno::NOENT,
    }
}

#[cfg(not(target_os = "macos"))]
fn linked_anywhere(_: u64, _: &Entry) -> bool {
    true
}

#[cfg(target_os = "macos")]
fn located(root: BorrowedFd<'_>, entry: &Entry) -> io::Result<Option<PathBuf>> {
    ignore::path_of(root, entry.id.ino)
}

#[cfg(not(target_os = "macos"))]
fn located(_: BorrowedFd<'_>, _: &Entry) -> io::Result<Option<PathBuf>> {
    Ok(None)
}

fn stat_rel(root: BorrowedFd<'_>, rel: &Path) -> io::Result<Stat> {
    let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) else {
        return Err(io::ErrorKind::NotFound.into());
    };
    match open_attached(root, parent, &mut |_| {})? {
        Some((dir, _)) => stat_at(dir.as_fd(), name),
        None => Err(io::ErrorKind::NotFound.into()),
    }
}

/// What one look at a folder found, for comparing two looks.
#[derive(Debug, PartialEq, Eq)]
enum Seen {
    /// No folder is at the path.
    Missing,
    /// The folder's names, the identity of every folder on the way to it, and
    /// its modification time, taken before the listing.
    Listed {
        names: BTreeSet<OsString>,
        chain: Vec<FileId>,
        mtime_ns: i128,
    },
}

/// Looks at the folder at `rel` under the root, or returns `None` if it cannot
/// be read.
fn observe(root: BorrowedFd<'_>, rel: &Path, cut: &mut dyn FnMut(Cut<'_>)) -> Option<Seen> {
    let (dir, chain) = match open_attached(root, rel, cut) {
        Ok(Some(found)) => found,
        Ok(None) => return Some(Seen::Missing),
        Err(_) => return None,
    };
    let mtime_ns = stat_fd(dir.as_fd()).ok()?.mtime_ns;
    let names = list(dir.as_fd()).ok()?;
    Some(Seen::Listed {
        names,
        chain,
        mtime_ns,
    })
}

/// Whether `name` in the folder at `parent` may still hold an entry of `kind`:
/// it does, or it cannot be read.
fn may_hold(
    root: BorrowedFd<'_>,
    parent: &Path,
    name: &OsStr,
    kind: EntryKind,
    cut: &mut dyn FnMut(Cut<'_>),
) -> bool {
    match open_attached(root, parent, cut) {
        Ok(Some((dir, _))) => match stat_at(dir.as_fd(), name) {
            Ok(stat) => stat.entry().is_some_and(|entry| entry.kind == kind),
            Err(err) => !is_gone(&err),
        },
        Ok(None) => false,
        Err(_) => true,
    }
}

/// Opens the folder at `rel` under the root one component at a time without
/// following links, with the identity of every folder opened on the way, or
/// returns `None` if no folder is there.
fn open_attached(
    root: BorrowedFd<'_>,
    rel: &Path,
    cut: &mut dyn FnMut(Cut<'_>),
) -> io::Result<Option<(OwnedFd, Vec<FileId>)>> {
    let mut dir = root.try_clone_to_owned()?;
    let mut path = PathBuf::new();
    let mut chain = Vec::new();
    for component in rel.components() {
        dir = match open_dir_at(dir.as_fd(), component.as_os_str()) {
            Ok(next) => next,
            Err(err) if is_gone(&err) => return Ok(None),
            Err(err) => return Err(err),
        };
        path.push(component);
        cut(Cut::Confirming(&path));
        chain.push(stat_fd(dir.as_fd())?.id);
    }
    Ok(Some((dir, chain)))
}

/// Takes out of `old` every entry the scan found left out at its path, with
/// everything below it, and returns their paths and the entries the snapshot
/// keeps as left out. An entry the scan did not find, left out or not, has
/// left the disk, so it stays in `old` to be reported as removed.
fn take_left_out(
    old: &mut BTreeMap<PathBuf, Entry>,
    found: &BTreeMap<PathBuf, Entry>,
) -> (Vec<PathBuf>, Vec<(PathBuf, Entry)>) {
    let mut left_out = Vec::new();
    let mut kept = Vec::new();
    let mut folder: Option<PathBuf> = None;
    old.retain(|path, entry| {
        if !folder
            .as_ref()
            .is_some_and(|folder| path.starts_with(folder))
        {
            let Some(now) = found.get(path).filter(|now| now.kind == entry.kind) else {
                return true;
            };
            kept.push((path.clone(), *now));
            folder = (entry.kind == EntryKind::Dir).then(|| path.clone());
        }
        left_out.push(path.clone());
        false
    });
    (left_out, kept)
}

#[cfg(test)]
thread_local! {
    static VISITED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static VISIT_BUDGET: std::cell::Cell<u64> = const { std::cell::Cell::new(u64::MAX) };
}

#[cfg(test)]
type Visiting<I> = std::iter::Inspect<I, fn(&<I as Iterator>::Item)>;

#[cfg(not(test))]
type Visiting<I> = I;

#[cfg(test)]
fn visiting<I: Iterator>(items: I) -> Visiting<I> {
    items.inspect(|_| {
        let visited = VISITED.get() + 1;
        VISITED.set(visited);
        let budget = VISIT_BUDGET.get();
        assert!(visited <= budget, "visited {visited}, budget {budget}");
    })
}

#[cfg(not(test))]
fn visiting<I: Iterator>(items: I) -> Visiting<I> {
    items
}

/// Turns each removal whose entry now sits at or below an entry the rules
/// leave out into [`Change::Ignored`] (design §5.10). A folder is looked for
/// by its own identity, and so is each entry it held. Such an entry found
/// there, and not inside a folder that was found, was moved out before its
/// folder went, and is reported as ignored beside the folder's removal. An
/// entry that a rename of it or of a folder above it carries to a path with
/// the same identity stayed included, so it is not looked for. A
/// removal the search does not find stands. So does a file another file took
/// the path of, as in a swap through a third name: when the old file sits at
/// a name the rules leave out, it is reported as ignored and the new one as
/// created. That search looks only at those names and not below them, since
/// a save by rename takes a path the same way and leaves no old file to find.
/// On macOS the system also names the path of the old file's identity, and
/// the old file found that way at or below such a name, as recorded, is
/// reported the same way. Elsewhere a file moved below such a name while
/// another took its path is a modification, as a save would be.
#[allow(clippy::too_many_arguments)]
fn find_ignored(
    changes: &mut Vec<Change>,
    old: &BTreeMap<PathBuf, Entry>,
    new: &BTreeMap<PathBuf, Entry>,
    beyond: &BTreeMap<PathBuf, Entry>,
    held: BTreeMap<PathBuf, Entry>,
    earlier: &Seeded,
    rules: &mut Pending<'_>,
    root: BorrowedFd<'_>,
    cut: &mut dyn FnMut(Cut<'_>),
) -> (Vec<PathBuf>, BTreeMap<PathBuf, Entry>) {
    let roots = outermost(held.keys().cloned().collect());
    let merged: BTreeMap<PathBuf, Entry>;
    let old = if held.is_empty() {
        old
    } else {
        let aside = |path: &Path| -> PathBuf {
            let (top, below) = visiting(roots.iter())
                .find_map(|top| Some((top, path.strip_prefix(top).ok()?)))
                .expect("held below a root");
            let mut name = top.as_os_str().to_owned();
            name.push("\0");
            Path::new(&name)
                .components()
                .chain(below.components())
                .collect()
        };
        merged = visiting(old.iter())
            .map(|(path, entry)| (path.clone(), *entry))
            .chain(held.iter().map(|(path, entry)| (aside(path), *entry)))
            .collect();
        *changes = diff(&merged, new, beyond);
        &merged
    };
    let (arrivals, postponed) = hold_back(changes, old, new, earlier, rules, root, cut);
    for change in visiting(changes.iter_mut()) {
        match change {
            Change::Removed { path, .. }
            | Change::Missing { path, .. }
            | Change::Renamed { from: path, .. }
            | Change::Ignored { from: path, .. } => *path = unaside(path),
            Change::Created { .. } | Change::Modified { .. } | Change::Reidentified { .. } => {}
        }
    }
    let created: BTreeSet<&PathBuf> = visiting(changes.iter())
        .filter_map(|change| match change {
            Change::Created {
                path,
                kind: EntryKind::File,
            } => Some(path),
            _ => None,
        })
        .collect();
    let saved: BTreeSet<PathBuf> = visiting(changes.iter())
        .filter_map(|change| match change {
            Change::Removed {
                path,
                kind: EntryKind::File,
            }
            | Change::Missing {
                path,
                kind: EntryKind::File,
                ..
            } if created.contains(path) => Some(path.clone()),
            _ => None,
        })
        .collect();
    changes
        .retain(|change| !matches!(change, Change::Created { path, .. } if saved.contains(path)));
    for change in visiting(changes.iter_mut()) {
        if let Change::Removed { path, .. } | Change::Missing { path, .. } = change
            && saved.contains(path)
        {
            *change = Change::Modified { path: path.clone() };
        }
    }
    (arrivals, postponed)
}

fn unaside(path: &Path) -> PathBuf {
    (path.iter())
        .map(|name| {
            let name = name.as_bytes();
            OsStr::from_bytes(name.strip_suffix(b"\0").unwrap_or(name))
        })
        .collect()
}

fn hold_back(
    changes: &mut Vec<Change>,
    old: &BTreeMap<PathBuf, Entry>,
    new: &BTreeMap<PathBuf, Entry>,
    earlier: &Seeded,
    rules: &mut Pending<'_>,
    root: BorrowedFd<'_>,
    cut: &mut dyn FnMut(Cut<'_>),
) -> (Vec<PathBuf>, BTreeMap<PathBuf, Entry>) {
    let replaced: BTreeSet<PathBuf> = visiting(changes.iter())
        .filter_map(|change| match change {
            Change::Modified { path }
                if (old.get(path).zip(new.get(path)))
                    .is_some_and(|(before, after)| !after.id.same(&before.id)) =>
            {
                Some(path.clone())
            }
            _ => None,
        })
        .collect();
    let removed: BTreeSet<PathBuf> = visiting(changes.iter())
        .filter_map(|change| match change {
            Change::Removed { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect();
    if removed.is_empty() && replaced.is_empty() {
        rules.hold(BTreeMap::new());
        return (Vec::new(), BTreeMap::new());
    }
    let renamed: HashMap<PathBuf, PathBuf> = visiting(changes.iter())
        .filter_map(|change| match change {
            Change::Renamed { from, to, .. } => Some((from.clone(), to.clone())),
            _ => None,
        })
        .collect();
    let carried = |path: &Path| {
        let entry = &old[path];
        visiting(path.ancestors())
            .filter_map(|from| Some((renamed.get(from)?, path.strip_prefix(from).ok()?)))
            .any(|(to, below)| {
                (new.get(&to.join(below)))
                    .is_some_and(|after| after.id.same(&entry.id) && after.kind == entry.kind)
            })
    };
    let mut held: BTreeSet<&PathBuf> = (removed.iter())
        .filter(|dir| old[*dir].kind == EntryKind::Dir)
        .flat_map(|dir| {
            visiting(old.range(dir.clone()..).skip(1))
                .take_while(move |(path, _)| path.starts_with(dir))
                .map(|(path, _)| path)
        })
        .filter(|path| !removed.contains(*path) && !carried(path))
        .collect();
    let key = |path: &Path| (old[path].id, old[path].kind);
    let mut overwrites: HashMap<Key, usize> = HashMap::new();
    for path in &replaced {
        *overwrites
            .entry((new[path].id, new[path].kind))
            .or_default() += 1;
    }
    let mut exempt: BTreeSet<&PathBuf> = BTreeSet::new();
    for path in removed.iter().chain(held.iter().copied()) {
        if let Some(left @ 1..) = overwrites.get_mut(&key(path)) {
            *left -= 1;
            exempt.insert(path);
        }
    }
    held.retain(|path| !exempt.contains(path));
    let searched: Vec<&PathBuf> = (removed.iter())
        .filter(|path| !exempt.contains(path))
        .collect();
    let gone: Vec<&PathBuf> = (searched.iter().copied())
        .chain(held.iter().copied())
        .collect();
    let mut wanted: Wanted = gone.iter().map(|path| (key(path), None)).collect();
    let fresh: HashSet<Key> = (gone.iter().copied())
        .filter(|path| !path.as_os_str().as_bytes().contains(&0))
        .chain(&replaced)
        .map(|path| key(path))
        .collect();
    let seeded: Vec<&PathBuf> = (replaced.iter())
        .filter(|path| earlier.unscanned(path))
        .collect();
    wanted.extend(seeded.iter().map(|path| (key(path), None)));
    let undecided = rules.search(root, &mut wanted, &fresh, true, cut);
    let relocated = |path: &Path| {
        unaside(
            &(path.ancestors().skip(1))
                .find_map(|above| Some(renamed.get(above)?.join(path.strip_prefix(above).ok()?)))
                .unwrap_or_else(|| path.to_path_buf()),
        )
    };
    let mut deferred: Vec<(PathBuf, PathBuf)> = (searched.iter().copied())
        .filter(|path| {
            wanted.get(&key(path)) == Some(&None)
                && visiting(held.iter().copied())
                    .filter(|inside| inside.starts_with(path))
                    .chain([*path])
                    .any(|inside| undecided.contains(&key(inside)))
        })
        .map(|path| (path.clone(), relocated(path)))
        .collect();
    let taken = |path: &Path| visiting(deferred.iter()).any(|(_, to)| path.starts_with(to));
    let mut arrivals: Vec<PathBuf> = visiting(changes.iter())
        .filter_map(|change| match change {
            Change::Created { path, .. } if taken(path) => Some(path.clone()),
            _ => None,
        })
        .collect();
    let finish = visiting(changes.iter()).any(|change| match change {
        Change::Modified { path } | Change::Renamed { to: path, .. } => taken(path),
        _ => false,
    });
    let mut unseen: Vec<PathBuf> = (seeded.iter())
        .filter(|path| undecided.contains(&key(path)))
        .map(|path| (*path).clone())
        .collect();
    if finish {
        rules.search(root, &mut wanted, &HashSet::new(), false, cut);
        deferred.clear();
        unseen.clear();
        arrivals.clear();
    }
    let unproven: HashSet<Key> = (wanted.keys().copied())
        .filter(|key| rules.unproven(*key))
        .collect();
    let gone_from_root =
        |entry: &Entry| cfg!(target_os = "macos") && matches!(located(root, entry), Ok(None));
    let overwrote: HashSet<Key> = (exempt.iter())
        .filter(|inside| (deferred.iter()).any(|(dir, _)| inside.starts_with(dir)))
        .map(|inside| key(inside))
        .collect();
    let postponed: BTreeMap<PathBuf, Entry> = (replaced.iter())
        .filter(|path| overwrote.contains(&(new[*path].id, new[*path].kind)))
        .map(|path| (path.clone(), old[path]))
        .collect();
    unseen.retain(|path| !postponed.contains_key(path));
    changes.retain(|change| match change {
        Change::Created { path, .. } => !visiting(arrivals.iter()).any(|arrival| arrival == path),
        Change::Modified { path } => {
            !visiting(unseen.iter()).any(|seen| seen == path) && !postponed.contains_key(path)
        }
        _ => true,
    });
    let mut kept: BTreeMap<PathBuf, Entry> = (unseen.iter())
        .map(|path| (path.clone(), old[path]))
        .collect();
    arrivals.extend(unseen);
    if !deferred.is_empty() {
        held.retain(|path| !deferred.iter().any(|(dir, _)| path.starts_with(dir)));
        changes.retain(|change| {
            !matches!(change, Change::Removed { path, .. } if deferred.iter().any(|(dir, _)| dir == path))
        });
        for (from, _) in &deferred {
            for (inside, entry) in (old.range(from.clone()..))
                .take_while(|(inside, _)| inside.starts_with(from))
                .filter(|(inside, _)| {
                    !(inside.ancestors())
                        .take_while(|above| above != from)
                        .any(|above| renamed.contains_key(above))
                })
            {
                kept.insert(relocated(inside), *entry);
            }
        }
    }
    rules.hold(kept);
    let mut swapped: Wanted = (replaced.iter())
        .filter(|path| !old[*path].linked)
        .map(|path| (key(path), None))
        .filter(|(id, _)| !wanted.contains_key(id))
        .collect();
    rules.find(root, &mut swapped);
    for path in &replaced {
        if let Some(found @ None) = swapped.get_mut(&key(path)) {
            *found = rules.locate(root, &old[path]);
        }
    }
    wanted.extend(swapped);
    let found = |path: &Path| {
        let entry = &old[path];
        (wanted.get(&(entry.id, entry.kind)).cloned().flatten()).map(|to| Change::Ignored {
            from: path.to_owned(),
            to,
            kind: entry.kind,
            id: entry.id,
        })
    };
    let mut moved: Vec<Change> = visiting(held.iter())
        .filter(|path| {
            !(path.ancestors().skip(1))
                .take_while(|above| old.contains_key(*above))
                .any(|above| found(above).is_some())
        })
        .filter_map(|path| found(path))
        .collect();
    for change in visiting(changes.iter_mut()) {
        match &*change {
            Change::Removed { path, kind } if !exempt.contains(&path) => {
                if let Some(ignored) = found(path) {
                    *change = ignored;
                } else if unproven.contains(&key(path)) && !gone_from_root(&old[path]) {
                    *change = Change::Missing {
                        path: path.clone(),
                        kind: *kind,
                        id: old[path].id,
                    };
                }
            }
            Change::Modified { path } if replaced.contains(path) => {
                if let Some(ignored) = found(path) {
                    *change = Change::Created {
                        path: path.clone(),
                        kind: EntryKind::File,
                    };
                    moved.push(ignored);
                }
            }
            _ => {}
        }
    }
    changes.extend(moved);
    (arrivals, postponed)
}

/// Sorts the paths and drops every path inside another one.
fn outermost(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort();
    paths.dedup();
    let mut outermost: Vec<PathBuf> = Vec::new();
    for path in paths {
        if !outermost.last().is_some_and(|last| path.starts_with(last)) {
            outermost.push(path);
        }
    }
    outermost
}

fn volume_of<'p>(
    path: &'p Path,
    tree: &BTreeMap<PathBuf, Entry>,
    beyond: &BTreeMap<PathBuf, Entry>,
) -> Option<&'p Path> {
    path.ancestors().find(|above| {
        (tree.get(*above).or_else(|| beyond.get(*above))).is_some_and(|entry| entry.mount.is_some())
    })
}

fn volume_now(
    path: &Path,
    old: &BTreeMap<PathBuf, Entry>,
    new: &BTreeMap<PathBuf, Entry>,
    beyond: &BTreeMap<PathBuf, Entry>,
) -> Option<PathBuf> {
    let volume = volume_of(path, old, beyond)?;
    Some(moved_to(volume, old, new, beyond).unwrap_or_else(|| volume.to_path_buf()))
}

fn carried(path: &Path, moved: &[(&Path, &Path)]) -> PathBuf {
    visiting(moved.iter())
        .filter(|(from, _)| path.starts_with(from))
        .max_by_key(|(from, _)| from.components().count())
        .map_or_else(
            || path.to_path_buf(),
            |(from, to)| to.join(path.strip_prefix(from).expect("below the moved folder")),
        )
}

fn diff<'t>(
    old: &'t BTreeMap<PathBuf, Entry>,
    new: &'t BTreeMap<PathBuf, Entry>,
    beyond: &'t BTreeMap<PathBuf, Entry>,
) -> Vec<Change> {
    let mounted = (old.values().chain(new.values()).chain(beyond.values()))
        .any(|entry| entry.mount.is_some());
    let volume = |path: &'t Path, tree| mounted.then(|| volume_of(path, tree, beyond)).flatten();
    let moved: HashSet<(Option<&Path>, FileId)> = visiting(old.iter())
        .filter(|(path, before)| {
            before.kind == EntryKind::Dir
                && !new
                    .get(*path)
                    .is_some_and(|after| after.kind == EntryKind::Dir)
        })
        .map(|(path, before)| (volume(path, old), before.id))
        .collect();
    let dirs: HashSet<(Option<&Path>, FileId)> = visiting(new.iter())
        .filter(|(_, after)| after.kind == EntryKind::Dir)
        .map(|(path, after)| (volume(path, new), after.id))
        .collect();
    let ids =
        |tree: &'t BTreeMap<PathBuf, Entry>| -> HashSet<(Option<&'t Path>, FileId, EntryKind)> {
            visiting(tree.iter())
                .map(|(path, entry)| (volume(path, tree), entry.id, entry.kind))
                .collect()
        };
    let (ids_before, ids_after) = (ids(old), ids(new));
    let kept = |path: &'t Path, before: &Entry, after: &Entry| {
        let (was_on, is_on) = (volume(path, old), volume(path, new));
        after.kind == before.kind
            && !(after.id != before.id
                && ids_before.contains(&(is_on, after.id, after.kind))
                && ids_after.contains(&(was_on, before.id, before.kind)))
            && !(after.kind == EntryKind::Dir
                && after.id != before.id
                && dirs.contains(&(was_on, before.id))
                && !visiting(old.range(path.to_path_buf()..).skip(1))
                    .take_while(|(inside, _)| inside.starts_with(path))
                    .any(|(inside, entry)| {
                        (new.get(inside))
                            .is_some_and(|after| (after.id, after.kind) == (entry.id, entry.kind))
                    }))
            && !(after.kind == EntryKind::Dir
                && after.id != before.id
                && moved.contains(&(is_on, after.id))
                && (dirs.contains(&(was_on, before.id))
                    || !old
                        .range(path.to_path_buf()..)
                        .nth(1)
                        .is_some_and(|(inside, _)| inside.starts_with(path))))
    };
    let mut changed: HashSet<&Path> = HashSet::new();
    for (path, before) in visiting(old.iter()) {
        let parent_changed = path.parent().is_some_and(|parent| changed.contains(parent));
        if parent_changed || !new.get(path).is_some_and(|after| kept(path, before, after)) {
            changed.insert(path);
        }
    }
    let mut removed: BTreeMap<&Path, &Entry> = BTreeMap::new();
    let mut created: Vec<(&Path, &Entry)> = Vec::new();
    let mut modified: Vec<&Path> = Vec::new();
    let mut reidentified: Vec<&Path> = Vec::new();
    for (path, before) in visiting(old.iter()) {
        match new.get(path) {
            Some(after) if !changed.contains(path.as_path()) => {
                let moved = (path.parent()).is_some_and(|parent| {
                    old.get(parent).map(|entry| entry.id) != new.get(parent).map(|entry| entry.id)
                });
                let differs = match moved {
                    true => after != before,
                    false => !before.same_version(after),
                };
                if after.kind == EntryKind::File && differs {
                    modified.push(path);
                } else if after.kind == EntryKind::Dir && after.id != before.id {
                    reidentified.push(path);
                }
            }
            _ => {
                removed.insert(path, before);
            }
        }
    }
    for (path, after) in visiting(new.iter()) {
        if !old.contains_key(path) || changed.contains(path.as_path()) {
            created.push((path, after));
        }
    }

    let mut by_id: HashMap<(FileId, EntryKind), VecDeque<&Path>> = HashMap::new();
    for (path, entry) in visiting(removed.iter()) {
        by_id
            .entry((entry.id, entry.kind))
            .or_default()
            .push_back(path);
    }
    let mut renames: Vec<(&Path, &Path, &Entry)> = Vec::new();
    let mut moved: Vec<(&Path, &Path)> = Vec::new();
    created.retain(|&(to, after)| {
        let target = volume_of(to, new, beyond);
        let Some(candidates) = by_id.get_mut(&(after.id, after.kind)) else {
            return true;
        };
        let Some(at) = visiting(candidates.iter()).position(|from| {
            let source = volume_of(from, old, beyond);
            (source == Some(*from) && target == Some(to))
                || source.map(|source| carried(source, &moved)) == target.map(Path::to_path_buf)
        }) else {
            return true;
        };
        let from = candidates.remove(at).expect("found among the candidates");
        let before = removed.remove(from).expect("indexed from removed");
        if after.kind == EntryKind::File && after != before {
            modified.push(to);
        }
        if after.kind == EntryKind::Dir {
            moved.push((from, to));
        }
        renames.push((from, to, after));
        false
    });
    renames.sort_by_key(|&(from, _, _)| from);
    modified.sort();

    let mut changes = Vec::new();
    for (&path, entry) in &removed {
        let parent_removed = path
            .parent()
            .is_some_and(|parent| removed.contains_key(parent));
        if !parent_removed {
            changes.push(Change::Removed {
                path: path.to_path_buf(),
                kind: entry.kind,
            });
        }
    }
    let mut moved_dirs: HashMap<&Path, &Path> = HashMap::new();
    for &(from, to, entry) in &renames {
        let implied = from.file_name() == to.file_name()
            && matches!(
                (from.parent(), to.parent()),
                (Some(a), Some(b)) if moved_dirs.get(a) == Some(&b)
            );
        if entry.kind == EntryKind::Dir {
            moved_dirs.insert(from, to);
        }
        if !implied {
            changes.push(Change::Renamed {
                from: from.to_path_buf(),
                to: to.to_path_buf(),
                kind: entry.kind,
            });
        }
    }
    for (path, entry) in created {
        changes.push(Change::Created {
            path: path.to_path_buf(),
            kind: entry.kind,
        });
    }
    for path in modified {
        changes.push(Change::Modified {
            path: path.to_path_buf(),
        });
    }
    for path in reidentified {
        changes.push(Change::Reidentified {
            path: path.to_path_buf(),
        });
    }
    changes
}

/// What one [`Watcher::poll`] found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Poll {
    /// The changes of one scan, already applied to [`Watcher::snapshot`].
    pub changes: Vec<Change>,
    /// Scopes the watcher knows changed but could not settle before the poll
    /// returned: a removal it could not confirm, or a scan that saw the tree
    /// change under it. A later poll rescans them.
    pub pending: Vec<PathBuf>,
    /// Folders the scans of this poll may not read; see
    /// [`Rescan::unreadable`].
    pub unreadable: Vec<PathBuf>,
    /// The identity of the root folder the scans of this poll opened, or
    /// `None` when the poll scanned nothing; see [`Rescan::root`].
    pub root: Option<FileId>,
    /// The rules files the scans of this poll found changed, in the order
    /// they found them; see [`Rescan::rules`].
    pub rules: Vec<(PathBuf, Source, Option<Vec<u8>>)>,
    /// The folders whose rules the scans of this poll established and that no
    /// later scan of the poll found unreadable; see [`Rescan::read`].
    pub read: Vec<PathBuf>,
    /// The files the scans of this poll found unchanged in content but whose change time moved, by any cause, to a new
    /// settled value or bound.
    pub settled: Vec<PathBuf>,
}

/// A source of changes under one root.
pub trait Watcher {
    /// Waits up to `timeout` for changes and returns them with the scopes still
    /// pending. Both empty means nothing changed; changes empty and scopes
    /// pending means work is outstanding. Constant activity in a scope can
    /// keep it pending across any number of polls, and a poll can run past
    /// `timeout` by one scan and, for [`NativeWatcher`], its settle window.
    fn poll(&mut self, timeout: Duration) -> Result<Poll>;

    /// The state the returned changes lead to. Keep it across restarts and
    /// pass it to the next watcher to catch changes made in between.
    fn snapshot(&self) -> &Snapshot;

    /// Replaces what the snapshot knows at and below `scope` with `known`, as
    /// [`Snapshot::reset`] does. The next poll rescans `scope` and reports
    /// how the disk differs from `known` there, and every file of `known` it
    /// finds as modified. Entries outside `scope` are not reported again
    /// unless they changed. A removal held at or below `scope` is left to
    /// `known`: that rescan holds each entry of `known` whose path now holds
    /// another identity until the search decides it, as the first scan of a
    /// new watcher does.
    fn reset(&mut self, scope: &Path, known: BTreeMap<PathBuf, Known>);

    /// Asks the next poll to scan `paths` again, such as files whose change time has not settled.
    fn revisit(&mut self, paths: Vec<PathBuf>);

    /// Looks for the entry of each path of `wanted` by kind and identity at and below every entry the rules leave out,
    /// and returns what it learned about each. `found` holds where an earlier call found each entry. The search goes
    /// on from one call to the next within the budget of a scan, and seeks only what the last call wanted.
    fn look_for(
        &mut self,
        wanted: &BTreeMap<PathBuf, Known>,
        found: &BTreeMap<PathBuf, PathBuf>,
    ) -> Result<Vec<(PathBuf, Looked)>>;
}

/// What [`Watcher::look_for`] learned about a wanted entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Looked {
    /// A search found it at this path.
    Found(PathBuf),
    /// A search that looked everywhere ([`Rules::coverage`]) did not find it, and the snapshot does not hold it.
    Gone,
    /// Another entry of its kind took the place where an earlier call found it, as a save by rename does, so it is
    /// the same entry with this identity. A scan counts a file that takes another's path the same way.
    Replaced(FileId),
}

struct Barrier {
    file: PathBuf,
    written: Instant,
    holds: BTreeSet<PathBuf>,
}

/// Watches with FSEvents on macOS and inotify on Linux.
///
/// When the platform reports lost events (an inotify queue overflow, or
/// FSEvents asking for a subtree rescan), the watcher re-registers the whole
/// root before rescanning it, so no folder found by the rescan is left
/// unwatched. On Linux it also registers each new or moved folder itself and
/// rescans it after the registration, so a write between the folder's creation
/// and its watch is not missed. If [`Watcher::poll`] fails with
/// [`Error::Watch`], the platform watcher cannot cover the tree (for example
/// the inotify watch limit); continue with a [`RescanWatcher`] built from
/// [`Watcher::snapshot`].
pub struct NativeWatcher {
    root: PathBuf,
    snapshot: Snapshot,
    view: OnceCell<Snapshot>,
    dirty: BTreeSet<PathBuf>,
    waiting: BTreeSet<PathBuf>,
    barrier: Option<Barrier>,
    barriers: u64,
    listed: BTreeSet<PathBuf>,
    unreadable: BTreeSet<PathBuf>,
    rules: Option<Rules>,
    /// The rules changes of the scans since the last poll returned.
    changed_rules: Vec<(PathBuf, Source, Option<Vec<u8>>)>,
    /// The folders whose rules the scans since the last poll established.
    read: BTreeSet<PathBuf>,
    /// Folders whose subfolders may be unwatched, which are registered once a
    /// scan has read them: the folders a scan could not read that no scan has
    /// read since, the root while it waits for a scan to read the rules, a
    /// folder a reset made known, which no scan reports as created.
    #[cfg(target_os = "linux")]
    blocked: BTreeSet<PathBuf>,
    uncovered: Option<(FileId, BTreeSet<PathBuf>)>,
    rewatch: bool,
    remount_checked: Instant,
    failed: Option<Error>,
    stopped: Box<dyn Fn() -> bool + Send>,
    found: Box<dyn Fn(u64) + Send>,
    marks: MarkLog,
    settled: BTreeSet<PathBuf>,
    sender: mpsc::Sender<notify::Result<notify::Event>>,
    events: mpsc::Receiver<notify::Result<notify::Event>>,
    watcher: RecommendedWatcher,
    #[cfg(all(test, target_os = "linux"))]
    before_register: Option<Box<dyn FnMut()>>,
}

impl NativeWatcher {
    /// Starts watching `root` recursively. The first poll compares the whole
    /// tree with `previous`; pass `Snapshot::default()` on a first run to have
    /// everything reported as created.
    pub fn start(root: impl AsRef<Path>, previous: Snapshot) -> Result<Self> {
        Self::start_under(root.as_ref(), previous, None)
    }

    /// [`NativeWatcher::start`], leaving out what `rules` leave out. The scans
    /// apply the rules, and an event below a left-out folder causes no rescan.
    /// On Linux no folder at or below a left-out folder is watched. Which
    /// folders those are is known once a scan has read the rules, so the
    /// watcher scans the tree, registers the folders the rules include, and
    /// scans once more for what changed before the watches existed.
    pub fn start_with_rules(
        root: impl AsRef<Path>,
        previous: Snapshot,
        rules: Rules,
    ) -> Result<Self> {
        Self::start_under(root.as_ref(), previous, Some(rules))
    }

    fn start_under(root: &Path, previous: Snapshot, rules: Option<Rules>) -> Result<Self> {
        let root = std::fs::canonicalize(root)?;
        let (sender, events) = mpsc::channel();
        let deferred = cfg!(target_os = "linux") && rules.is_some();
        let watcher = watch_root(&root, &sender, deferred)?;
        Ok(Self {
            snapshot: previous,
            root,
            view: OnceCell::new(),
            dirty: BTreeSet::from([PathBuf::new()]),
            waiting: BTreeSet::new(),
            barrier: None,
            barriers: 0,
            listed: BTreeSet::new(),
            unreadable: BTreeSet::new(),
            #[cfg(target_os = "linux")]
            blocked: BTreeSet::from_iter(deferred.then(PathBuf::new)),
            uncovered: None,
            rules,
            changed_rules: Vec::new(),
            read: BTreeSet::new(),
            rewatch: false,
            remount_checked: Instant::now(),
            failed: None,
            stopped: Box::new(|| false),
            found: Box::new(|_| {}),
            marks: MarkLog::default(),
            settled: BTreeSet::new(),
            sender,
            events,
            watcher,
            #[cfg(all(test, target_os = "linux"))]
            before_register: None,
        })
    }

    /// Makes a poll fail with [`Error::Stopped`] once `stopped` holds, at the
    /// next entry its scans reach, so a caller that stops does not wait for a
    /// scan of the whole tree. The snapshot stays as the last poll that
    /// returned left it, and the scopes it was scanning stay pending.
    pub fn stopping_when(self, stopped: impl Fn() -> bool + Send + 'static) -> Self {
        Self {
            stopped: Box::new(stopped),
            ..self
        }
    }

    /// Calls `found` with the number of files a scan has found so far, at
    /// each file it finds, so a caller can show how far a long scan has got.
    /// Each scan counts from zero.
    pub fn counting(self, found: impl Fn(u64) + Send + 'static) -> Self {
        Self {
            found: Box::new(found),
            ..self
        }
    }

    /// Reads no change into a change time that a mark `marks` records moved.
    pub fn marking(self, marks: MarkLog) -> Self {
        Self { marks, ..self }
    }

    fn note(&mut self, event: notify::Result<notify::Event>) -> Result<()> {
        let event = event.map_err(watch_error)?;
        if event.need_rescan() {
            self.rewatch = true;
            self.dirty.insert(PathBuf::new());
            self.barrier = None;
            self.dirty.append(&mut self.waiting);
        }
        if let EventKind::Access(access) = event.kind
            && access != AccessKind::Close(AccessMode::Write)
        {
            return Ok(());
        }
        let written = matches!(
            event.kind,
            EventKind::Create(_)
                | EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Any)
                | EventKind::Access(AccessKind::Close(AccessMode::Write))
        );
        if written
            && (self.barrier.as_ref()).is_some_and(|barrier| event.paths.contains(&barrier.file))
        {
            self.lift_barrier();
        }
        let listing_only = match event.kind {
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::Extended)) => true,
            // FSEvents gives an event every flag its path collected lately, so a mark on a folder made moments before
            // also reports the folder's creation:
            // https://developer.apple.com/documentation/coreservices/1455361-fseventstreameventflags
            #[cfg(target_vendor = "apple")]
            EventKind::Create(_) => true,
            // inotify reports an extended attribute written on a folder as IN_ATTRIB, as it does a chmod:
            // https://man7.org/linux/man-pages/man7/inotify.7.html
            #[cfg(target_os = "linux")]
            EventKind::Modify(ModifyKind::Metadata(_)) => true,
            _ => false,
        };
        for path in event.paths {
            let rel = path.strip_prefix(&self.root).unwrap_or(Path::new(""));
            if listing_only && self.folder_as_scanned(&path, rel) {
                self.listed.insert(rel.to_path_buf());
                continue;
            }
            // An event for a left-out entry of an included folder is kept: the
            // rescan of that folder records the name as left out.
            let below_left_out = self.rules.as_ref().is_some_and(|rules| {
                rel.parent()
                    .is_some_and(|parent| rules.status(parent, EntryKind::Dir) != Ignored::No)
            });
            if !below_left_out {
                self.dirty.insert(rel.to_path_buf());
            }
        }
        Ok(())
    }

    fn folder_as_scanned(&self, path: &Path, rel: &Path) -> bool {
        #[cfg(target_os = "linux")]
        if self.blocked.contains(rel) {
            return false;
        }
        (self.snapshot.entries.get(rel))
            .filter(|entry| entry.kind == EntryKind::Dir)
            .is_some_and(|scanned| {
                stat_path(path).is_ok_and(|stat| stat.entry().as_ref() == Some(scanned))
            })
    }

    fn lift_barrier(&mut self) {
        let Some(barrier) = self.barrier.take() else {
            return;
        };
        for path in barrier.holds {
            if self.waiting.remove(&path) {
                self.dirty.insert(path);
            }
        }
        self.raise_barrier();
    }

    fn raise_barrier(&mut self) {
        if self.barrier.is_some() || self.waiting.is_empty() {
            return;
        }
        self.barriers += 1;
        let name = format!("{BARRIER}-{}", self.barriers % 2);
        #[cfg(target_os = "linux")]
        let watched = notify::Watcher::watch(
            &mut self.watcher,
            &self.root.join(PRIVATE_DIR),
            RecursiveMode::NonRecursive,
        )
        .is_ok();
        #[cfg(not(target_os = "linux"))]
        let watched = true;
        if !(watched && write_barrier(&self.root, &name).is_ok()) {
            self.dirty.append(&mut self.waiting);
            return;
        }
        self.barrier = Some(Barrier {
            file: self.root.join(PRIVATE_DIR).join(name),
            written: Instant::now(),
            holds: self.waiting.clone(),
        });
    }

    fn receive(&mut self, timeout: Duration) -> Result<bool> {
        match self.events.recv_timeout(timeout) {
            Ok(event) => self.note(event).map(|()| true),
            Err(RecvTimeoutError::Timeout) => Ok(false),
            Err(RecvTimeoutError::Disconnected) => Err(watch_error("the platform watcher stopped")),
        }
    }

    /// Records the folders a rescan of `scopes` could not read, and returns
    /// the blocked folders it read again.
    #[cfg(target_os = "linux")]
    fn unblock(&mut self, scopes: &[PathBuf], unreadable: &[PathBuf]) -> Vec<PathBuf> {
        let readable: Vec<PathBuf> = self
            .blocked
            .iter()
            .filter(|blocked| scopes.iter().any(|scope| blocked.starts_with(scope)))
            .filter(|blocked| !unreadable.iter().any(|path| blocked.starts_with(path)))
            .cloned()
            .collect();
        for path in &readable {
            self.blocked.remove(path);
        }
        self.blocked.extend(unreadable.iter().cloned());
        readable
    }

    #[cfg(target_os = "linux")]
    fn cover(&mut self, changes: &[Change], readable: Vec<PathBuf>, root: FileId) {
        let dirs = outermost(
            changes
                .iter()
                .filter_map(|change| match change {
                    Change::Created {
                        path,
                        kind: EntryKind::Dir,
                    }
                    | Change::Renamed {
                        to: path,
                        kind: EntryKind::Dir,
                        ..
                    }
                    | Change::Reidentified { path } => Some(path.clone()),
                    _ => None,
                })
                .chain(readable)
                .collect(),
        );
        if dirs.is_empty() {
            return;
        }
        #[cfg(test)]
        if let Some(hook) = self.before_register.as_mut() {
            hook();
        }
        for dir in &dirs {
            let path = self.root.join(dir);
            let registered = match &self.rules {
                Some(rules) => watch_each(&mut self.watcher, &path, &|folder| {
                    !(self.stopped)()
                        && folder
                            .strip_prefix(&self.root)
                            .is_ok_and(|rel| rules.status(rel, EntryKind::Dir) == Ignored::No)
                }),
                None => watch_tree(&mut self.watcher, &path),
            };
            if let Err(err) = registered
                && !gone(&path)
            {
                self.failed = Some(watch_error(err));
            }
        }
        if (self.stopped)() {
            self.blocked.extend(dirs.iter().cloned());
            self.dirty.extend(dirs);
            return;
        }
        self.uncovered = Some((root, dirs.into_iter().collect()));
    }
}

impl NativeWatcher {
    /// What a poll returns once it has `changes` from a scan of `root`: the
    /// scopes still dirty, and the folders its scans could not read or read.
    fn report(&mut self, changes: Vec<Change>, root: Option<FileId>) -> Poll {
        Poll {
            changes,
            pending: self.dirty.union(&self.waiting).cloned().collect(),
            unreadable: std::mem::take(&mut self.unreadable).into_iter().collect(),
            root,
            rules: std::mem::take(&mut self.changed_rules),
            read: std::mem::take(&mut self.read).into_iter().collect(),
            settled: std::mem::take(&mut self.settled).into_iter().collect(),
        }
    }

    fn came_back(&self, path: &Path) -> bool {
        let (Some(known), Some(parent)) = (self.snapshot.entries.get(path), path.parent()) else {
            return false;
        };
        let (Ok(dir), Ok(above)) = (
            open_root(&self.root.join(path)),
            stat_path(&self.root.join(parent)),
        ) else {
            return false;
        };
        stat_fd(dir.as_fd()).is_ok_and(|now| {
            let mount = (now.dev != above.dev).then(|| mount_at(dir.as_fd()));
            let found = Entry {
                id: now.id,
                mount: mount.map(|(mount, _)| mount),
                ..*known
            };
            known.is_back(&found, mount.and_then(|(_, plain)| plain))
        })
    }

    /// Takes in which folders a scan read and which it could not read. The
    /// later scan decides for a folder that two scans of a poll judged.
    fn learn(&mut self, read: Vec<PathBuf>, unreadable: &[PathBuf]) {
        for dir in read {
            self.unreadable.remove(&dir);
            self.read.insert(dir);
        }
        for dir in unreadable {
            self.read.remove(dir);
            self.unreadable.insert(dir.clone());
        }
    }
}

fn write_barrier(root: &Path, name: &str) -> io::Result<()> {
    let private = open_dir_at(open_root(root)?.as_fd(), OsStr::new(PRIVATE_DIR))?;
    let flags =
        OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let file = rustix::fs::openat(&private, name, flags, Mode::from_raw_mode(0o644))?;
    rustix::io::write(&file, b"\n")?;
    Ok(())
}

/// Starts a platform watcher on `root` and everything below it, or, when the
/// folders below it are `deferred` to [`NativeWatcher::cover`], on the root
/// folder alone.
fn watch_root(
    root: &Path,
    sender: &mpsc::Sender<notify::Result<notify::Event>>,
    deferred: bool,
) -> Result<RecommendedWatcher> {
    let mut watcher = notify::recommended_watcher(sender.clone()).map_err(watch_error)?;
    if deferred {
        notify::Watcher::watch(&mut watcher, root, RecursiveMode::NonRecursive)
    } else {
        watch_tree(&mut watcher, root)
    }
    .map_err(watch_error)?;
    Ok(watcher)
}

/// Registers `path` and every folder below it that may be watched. The
/// platform's recursive registration stops at the first folder it may not
/// watch, or at one that disappears while it registers the tree, and the error
/// can name a folder deep inside it. After such an error every folder is
/// registered on its own instead, so each error names the folder it was asked
/// for. A folder that may not be read is left to the scans, which report it as
/// unreadable. A not-found error is skipped only once the folder it names is
/// checked again and is gone; if `path` itself is gone, the error is returned.
fn watch_tree<W: notify::Watcher>(watcher: &mut W, path: &Path) -> notify::Result<()> {
    match watcher.watch(path, RecursiveMode::Recursive) {
        Err(err) if refused(&err) || (vanished(&err) && !gone(path)) => {
            watch_each(watcher, path, &|_| true)
        }
        other => other,
    }
}

/// Registers `path` and every folder below it one at a time, skipping a
/// folder that may not be watched or that is gone, and a folder `keep`
/// refuses with everything below it.
fn watch_each<W: notify::Watcher>(
    watcher: &mut W,
    path: &Path,
    keep: &dyn Fn(&Path) -> bool,
) -> notify::Result<()> {
    if !keep(path) {
        return Ok(());
    }
    match watcher.watch(path, RecursiveMode::NonRecursive) {
        Err(err) if refused(&err) || (vanished(&err) && gone(path)) => return Ok(()),
        other => other?,
    }
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(err) if denied(&err) || (err.kind() == io::ErrorKind::NotFound && gone(path)) => {
            return Ok(());
        }
        Err(err) => return Err(notify::Error::io(err).add_path(path.to_path_buf())),
    };
    for entry in entries {
        let entry = entry.map_err(|err| notify::Error::io(err).add_path(path.to_path_buf()))?;
        let folder = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => watch_each(watcher, &folder, keep)?,
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound && gone(&folder) => {}
            Err(err) => return Err(notify::Error::io(err).add_path(folder)),
        }
    }
    Ok(())
}

/// Whether a lookup shows that `path` is no longer a folder: it is missing,
/// something other than a folder is there, or a path component is not a
/// folder. A lookup that fails for any other reason, such as a parent that may
/// not be searched, says nothing, so the folder is not taken to be gone.
fn gone(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => !meta.is_dir(),
        Err(err) => matches!(
            err.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
        ),
    }
}

fn refused(err: &notify::Error) -> bool {
    matches!(&err.kind, notify::ErrorKind::Io(err) if denied(err))
}

fn vanished(err: &notify::Error) -> bool {
    match &err.kind {
        notify::ErrorKind::PathNotFound => true,
        notify::ErrorKind::Io(err) => err.kind() == io::ErrorKind::NotFound,
        _ => false,
    }
}

impl Watcher for NativeWatcher {
    fn poll(&mut self, timeout: Duration) -> Result<Poll> {
        self.view.take();
        let deadline = Instant::now() + timeout;
        let mut root = None;
        loop {
            if let Some(err) = self.failed.take() {
                return Err(err);
            }
            if self.dirty.is_empty() && self.listed.is_empty() && self.uncovered.is_none() {
                let left = deadline.saturating_duration_since(Instant::now());
                let wait = if self.snapshot.detached.is_empty() {
                    left
                } else {
                    left.min(REMOUNT_CHECK)
                };
                let wait = match &self.barrier {
                    Some(barrier) => {
                        wait.min(BARRIER_TIMEOUT.saturating_sub(barrier.written.elapsed()))
                    }
                    None => wait,
                };
                self.receive(wait)?;
            }
            if (self.barrier.as_ref())
                .is_some_and(|barrier| barrier.written.elapsed() >= BARRIER_TIMEOUT)
            {
                self.lift_barrier();
            }
            if !self.snapshot.detached.is_empty() && self.remount_checked.elapsed() >= REMOUNT_CHECK
            {
                self.remount_checked = Instant::now();
                let back: Vec<PathBuf> = (self.snapshot.detached.keys())
                    .filter(|path| self.came_back(path))
                    .cloned()
                    .collect();
                self.dirty.extend(back);
            }
            if self.dirty.is_empty() && self.listed.is_empty() && self.uncovered.is_none() {
                if Instant::now() >= deadline {
                    return Ok(self.report(Vec::new(), root));
                }
                continue;
            }
            let settle_until = Instant::now() + MAX_SETTLE;
            while Instant::now() < settle_until && self.receive(SETTLE)? {}
            if self.rewatch {
                let deferred = cfg!(target_os = "linux") && self.rules.is_some();
                self.watcher = watch_root(&self.root, &self.sender, deferred)?;
                #[cfg(target_os = "linux")]
                self.blocked.extend(deferred.then(PathBuf::new));
                self.rewatch = false;
            }
            let held = self.snapshot.unplaced.clone();
            let (expected, uncovered) = self.uncovered.clone().unzip();
            let scopes: Vec<PathBuf> = (self.dirty.iter())
                .chain(uncovered.iter().flatten())
                .cloned()
                .collect();
            let rescan = match self.snapshot.rescan_under(
                &self.root,
                scopes.iter().cloned(),
                &self.listed,
                expected,
                self.rules.as_mut(),
                &*self.stopped,
                &*self.found,
                Keep::Unexplained {
                    waiting: &self.waiting,
                },
                &mut |_| {},
                &self.marks,
            ) {
                Ok(rescan) => rescan,
                Err(Error::Changed) if Instant::now() < deadline => continue,
                Err(Error::Changed) => return Ok(self.report(Vec::new(), root)),
                Err(Error::RootReplaced) => {
                    self.uncovered = None;
                    return Err(Error::RootReplaced);
                }
                Err(err) => return Err(err),
            };
            self.uncovered = None;
            self.listed.clear();
            root = Some(rescan.root);
            self.changed_rules.extend(rescan.rules);
            self.settled.extend(rescan.settled);
            self.waiting
                .retain(|path| self.snapshot.unplaced.contains(path));
            if let Some(barrier) = self.barrier.as_mut() {
                barrier.holds.retain(|path| self.waiting.contains(path));
            }
            self.waiting
                .extend(self.snapshot.unplaced.difference(&held).cloned());
            self.dirty = (rescan.unsettled.into_iter())
                .filter(|path| !self.waiting.contains(path))
                .collect();
            self.raise_barrier();
            #[cfg(target_os = "linux")]
            let readable = self.unblock(&scopes, &rescan.unreadable);
            self.learn(rescan.read, &rescan.unreadable);
            let changes = rescan.changes;
            #[cfg(target_os = "linux")]
            self.cover(&changes, readable, rescan.root);
            if !changes.is_empty()
                || !self.changed_rules.is_empty()
                || !self.settled.is_empty()
                || Instant::now() >= deadline
                || !self.snapshot.unplaced.is_subset(&held)
            {
                return Ok(self.report(changes, root));
            }
        }
    }

    fn snapshot(&self) -> &Snapshot {
        view(&self.snapshot, self.rules.as_ref(), &self.view)
    }

    fn reset(&mut self, scope: &Path, known: BTreeMap<PathBuf, Known>) {
        #[cfg(target_os = "linux")]
        if scope.as_os_str().is_empty()
            || (known.get(scope)).is_some_and(|known| known.kind == EntryKind::Dir)
        {
            self.blocked.insert(scope.to_path_buf());
        }
        if let Some(rules) = &mut self.rules {
            rules.forget(scope);
        }
        self.view.take();
        self.snapshot.reset(scope, known);
        self.dirty.insert(scope.to_path_buf());
    }

    fn revisit(&mut self, paths: Vec<PathBuf>) {
        self.dirty.extend(paths);
    }

    fn look_for(
        &mut self,
        wanted: &BTreeMap<PathBuf, Known>,
        found: &BTreeMap<PathBuf, PathBuf>,
    ) -> Result<Vec<(PathBuf, Looked)>> {
        (self.snapshot).look_for(&self.root, self.rules.as_mut(), wanted, found)
    }
}

/// Rescans the whole tree every `interval`. Slower than [`NativeWatcher`] on
/// large trees, but needs nothing from the platform.
pub struct RescanWatcher {
    root: PathBuf,
    snapshot: Snapshot,
    view: OnceCell<Snapshot>,
    interval: Duration,
    rules: Option<Rules>,
    stopped: Box<dyn Fn() -> bool + Send>,
    found: Box<dyn Fn(u64) + Send>,
    marks: MarkLog,
}

impl RescanWatcher {
    pub fn new(root: impl AsRef<Path>, previous: Snapshot, interval: Duration) -> Result<Self> {
        let root = std::fs::canonicalize(root)?;
        Ok(Self {
            snapshot: previous,
            root,
            view: OnceCell::new(),
            interval,
            rules: None,
            stopped: Box::new(|| false),
            found: Box::new(|_| {}),
            marks: MarkLog::default(),
        })
    }

    /// [`RescanWatcher::new`], with scans that leave out what `rules` leave
    /// out.
    pub fn with_rules(
        root: impl AsRef<Path>,
        previous: Snapshot,
        interval: Duration,
        rules: Rules,
    ) -> Result<Self> {
        Ok(Self {
            rules: Some(rules),
            ..Self::new(root, previous, interval)?
        })
    }

    /// Makes a poll fail with [`Error::Stopped`] once `stopped` holds, as
    /// [`NativeWatcher::stopping_when`] does. The snapshot stays as the last
    /// poll that returned left it.
    pub fn stopping_when(self, stopped: impl Fn() -> bool + Send + 'static) -> Self {
        Self {
            stopped: Box::new(stopped),
            ..self
        }
    }

    /// Calls `found` as [`NativeWatcher::counting`] does.
    pub fn counting(self, found: impl Fn(u64) + Send + 'static) -> Self {
        Self {
            found: Box::new(found),
            ..self
        }
    }

    /// As [`NativeWatcher::marking`] does.
    pub fn marking(self, marks: MarkLog) -> Self {
        Self { marks, ..self }
    }
}

impl Watcher for RescanWatcher {
    fn poll(&mut self, timeout: Duration) -> Result<Poll> {
        self.view.take();
        let deadline = Instant::now() + timeout;
        loop {
            let rescan = self.snapshot.rescan_under(
                &self.root,
                [PathBuf::new()],
                &BTreeSet::new(),
                None,
                self.rules.as_mut(),
                &*self.stopped,
                &*self.found,
                Keep::Unplaced,
                &mut |_| {},
                &self.marks,
            );
            let poll = match rescan {
                Ok(rescan) => Poll {
                    changes: rescan.changes,
                    pending: rescan.unsettled,
                    unreadable: rescan.unreadable,
                    root: Some(rescan.root),
                    rules: rescan.rules,
                    read: rescan.read,
                    settled: rescan.settled,
                },
                Err(Error::Changed) => Poll {
                    pending: vec![PathBuf::new()],
                    ..Poll::default()
                },
                Err(err) => return Err(err),
            };
            let left = deadline.saturating_duration_since(Instant::now());
            if !poll.changes.is_empty()
                || !poll.rules.is_empty()
                || !poll.settled.is_empty()
                || left.is_zero()
            {
                return Ok(poll);
            }
            if !self.rules.as_ref().is_some_and(Rules::searching) {
                std::thread::sleep(self.interval.min(left));
            }
        }
    }

    fn snapshot(&self) -> &Snapshot {
        view(&self.snapshot, self.rules.as_ref(), &self.view)
    }

    fn reset(&mut self, scope: &Path, known: BTreeMap<PathBuf, Known>) {
        if let Some(rules) = &mut self.rules {
            rules.forget(scope);
        }
        self.view.take();
        self.snapshot.reset(scope, known);
    }

    fn revisit(&mut self, _paths: Vec<PathBuf>) {}

    fn look_for(
        &mut self,
        wanted: &BTreeMap<PathBuf, Known>,
        found: &BTreeMap<PathBuf, PathBuf>,
    ) -> Result<Vec<(PathBuf, Looked)>> {
        (self.snapshot).look_for(&self.root, self.rules.as_mut(), wanted, found)
    }
}

fn view<'a>(
    snapshot: &'a Snapshot,
    rules: Option<&Rules>,
    view: &'a OnceCell<Snapshot>,
) -> &'a Snapshot {
    let Some(held) = rules.and_then(Rules::held) else {
        return snapshot;
    };
    view.get_or_init(|| {
        let mut view = snapshot.clone();
        view.entries
            .extend(held.iter().map(|(path, entry)| (path.clone(), *entry)));
        view
    })
}

fn watch_error(err: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Error {
    Error::Watch(err.into())
}

/// A path or a name as a log line, an error or a status sentence shows it: in quotes, as `{:?}` writes it, so that a
/// reader, and `koi diagnose`, can tell where it ends whatever it holds.
pub fn quoted<P: AsRef<Path> + ?Sized>(path: &P) -> impl std::fmt::Display + '_ {
    struct Quoted<'a>(&'a Path);
    impl std::fmt::Display for Quoted<'_> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{:?}", self.0)
        }
    }
    Quoted(path.as_ref())
}

/// A path relative to a linked folder, held as text for status, decisions, restores and share links. It has no
/// [`Display`](std::fmt::Display): text a person reads shows it through [`quoted`].
#[derive(
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct ItemPath(String);

impl ItemPath {
    /// The path as it is, for machine-readable output and for the app, which shows it itself.
    pub fn unquoted(&self) -> &str {
        &self.0
    }
}

impl AsRef<Path> for ItemPath {
    fn as_ref(&self) -> &Path {
        Path::new(&self.0)
    }
}

impl From<String> for ItemPath {
    fn from(path: String) -> Self {
        Self(path)
    }
}

impl From<&str> for ItemPath {
    fn from(path: &str) -> Self {
        Self(path.to_owned())
    }
}

impl PartialEq<str> for ItemPath {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for ItemPath {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl From<ItemPath> for String {
    fn from(path: ItemPath) -> Self {
        path.0
    }
}

mod effect;
mod ignore;
mod mark;
mod recovered;
mod resolve;
mod root;
#[cfg(test)]
mod tests;
mod trash;

pub use effect::{
    Action, BroughtBack, Contents, Displaced, Effect, Outcome, QUARANTINE_DIR, Reason, Refusal,
    STAGING_DIR, Section, Step,
};
pub use ignore::{Coverage, Ignore, Ignored, Rules};
use ignore::{Key, Pending, RulesRead, Seeded, Wanted};
pub use koidrive_ignore::Source;
pub use mark::{MarkLog, node_mark};
pub use recovered::{recovered_name, utc_date};
pub use resolve::{Base, Dir, Folder, Location, PARKED_DIR, Space, Unresolved};
pub use root::{
    LINEAGE_KEPT, Lineage, Marker, Minted, PRIVATE_DIR, RootAt, RootProblem, RootTokens, Rotation,
    VolumeId, check_root, check_root_at, open_root_folder, probe_no_replace, read_marker,
    rename_no_replace, root_at, write_marker,
};
pub use trash::{Reclaimed, TRASH_DIR, TRASH_RETENTION};
