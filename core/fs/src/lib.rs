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
//! keeps its old entry and is returned for another rescan.
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
//! file or the device's own list excludes, with everything below it. A scan
//! reads the [`RULES_FILE`] of each folder it lists and hands the bytes to
//! the caller's matcher ([`Ignore`]) before it judges the folder's entries.
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
//! and is missed. Design §5.10 holds a miss for a question to the user while
//! any entry is left out, and the engine cannot ask that question yet. The
//! search does not look in [`PRIVATE_DIR`] or in the built-in platform files.
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

use notify::event::{AccessKind, AccessMode, EventKind};
use notify::{RecommendedWatcher, RecursiveMode};
use rustix::fs::{FileType, Mode, OFlags, RawMode};

/// How long native events must be quiet before the watcher rescans, so both
/// halves of a rename and a create followed by a write land in one batch.
const SETTLE: Duration = Duration::from_millis(50);
/// The longest a batch waits for quiet under constant activity.
const MAX_SETTLE: Duration = Duration::from_secs(1);

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
/// again. The birth time guards against inode numbers being reused, on file
/// systems that report it. `ino` is 128 bits wide because ReFS file IDs are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId {
    pub dev: u64,
    pub ino: u128,
    pub birth_ns: Option<i64>,
}

/// One file or folder. For folders, `size`, `mtime_ns` and `executable` are
/// always zero: a folder changes only through its children.
#[derive(Debug, Clone, Copy)]
pub struct Entry {
    pub kind: EntryKind,
    pub id: FileId,
    pub size: u64,
    pub mtime_ns: i128,
    pub executable: bool,
    /// Whether the file may have another link: it had one when it was
    /// scanned, or it was not scanned. Two entries that differ only here are
    /// equal, since a link made or removed elsewhere changes nothing at this
    /// path.
    pub linked: bool,
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        (
            self.kind,
            self.id,
            self.size,
            self.mtime_ns,
            self.executable,
        ) == (
            other.kind,
            other.id,
            other.size,
            other.mtime_ns,
            other.executable,
        )
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
                executable: false,
                linked: false,
            },
            FileType::RegularFile => Entry {
                kind: EntryKind::File,
                id: self.id,
                size: self.size,
                mtime_ns: self.mtime_ns,
                executable: self.mode & 0o111 != 0,
                linked: self.nlink > 1,
            },
            _ => return None,
        })
    }

    fn same_inode(&self, id: &FileId) -> bool {
        self.id.dev == id.dev && self.id.ino == id.ino
    }
}

#[cfg(target_os = "linux")]
fn statx(dir: BorrowedFd<'_>, name: &OsStr, flags: rustix::fs::AtFlags) -> io::Result<Stat> {
    use rustix::fs::StatxFlags;
    let st = rustix::fs::statx(
        dir,
        name,
        flags,
        StatxFlags::BASIC_STATS | StatxFlags::BTIME,
    )?;
    let nanos = |sec: i64, nsec: u32| i128::from(sec) * 1_000_000_000 + i128::from(nsec);
    let birth_ns = (st.stx_mask & StatxFlags::BTIME.bits() != 0)
        .then(|| i64::try_from(nanos(st.stx_btime.tv_sec, st.stx_btime.tv_nsec)).ok())
        .flatten();
    Ok(Stat {
        file_type: FileType::from_raw_mode(RawMode::from(st.stx_mode)),
        id: FileId {
            dev: rustix::fs::makedev(st.stx_dev_major, st.stx_dev_minor),
            ino: st.stx_ino.into(),
            birth_ns,
        },
        size: st.stx_size,
        mtime_ns: nanos(st.stx_mtime.tv_sec, st.stx_mtime.tv_nsec),
        ctime_ns: nanos(st.stx_ctime.tv_sec, st.stx_ctime.tv_nsec),
        mode: u32::from(st.stx_mode),
        nlink: u64::from(st.stx_nlink),
    })
}

/// Stats `name` inside `dir` without following a symbolic link.
#[cfg(target_os = "linux")]
fn stat_at(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<Stat> {
    statx(dir, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
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
            dev: st.st_dev as u64,
            ino: st.st_ino.into(),
            birth_ns: i64::try_from(nanos(st.st_birthtime, st.st_birthtime_nsec)).ok(),
        },
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

/// Where a scan can be interrupted in tests.
#[cfg_attr(not(test), allow(dead_code))]
enum Cut<'a> {
    RootOpened,
    OpeningDir(&'a Path),
    /// A removal's confirmation just opened this folder.
    Confirming(&'a Path),
    Searching(&'a Path),
}

/// The entries one rescan observed, and the parts of the snapshot they replace.
struct Scan<'c> {
    new: BTreeMap<PathBuf, Entry>,
    /// Every folder the scan opened, with the identity it had when opened.
    /// Each must still be at its path when the scan is published.
    bindings: Vec<(PathBuf, FileId)>,
    /// Paths whose old entry and everything below it give way to `new`.
    replaced: Vec<PathBuf>,
    /// Folders the scan may not open or list. What lies below them stays as
    /// it was known.
    unreadable: Vec<PathBuf>,
    left_out: BTreeMap<PathBuf, Entry>,
    /// The rules that leave entries out, for a scan that applies any.
    rules: Option<Pending<'c>>,
    cut: &'c mut dyn FnMut(Cut<'_>),
    /// Whether the caller has stopped. The scan fails with [`Error::Stopped`]
    /// at the next entry once it holds.
    stopped: &'c dyn Fn() -> bool,
}

impl Scan<'_> {
    /// Records the entry `name` of `dir` at `rel`, and everything below it.
    fn entry(&mut self, dir: BorrowedFd<'_>, name: &OsStr, rel: PathBuf) -> Result<()> {
        if (self.stopped)() {
            return Err(Error::Stopped);
        }
        let stat = match stat_at(dir, name) {
            Ok(stat) => stat,
            Err(err) if is_gone(&err) => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        let Some(entry) = stat.entry() else {
            return Ok(());
        };
        if let Some(rules) = &mut self.rules
            && rules.leaves_out(&rel, entry.kind)
        {
            self.left_out.insert(rel, entry);
            return Ok(());
        }
        self.new.insert(rel.clone(), entry);
        if entry.kind == EntryKind::File {
            return Ok(());
        }
        (self.cut)(Cut::OpeningDir(&rel));
        let sub = match open_dir_at(dir, name) {
            Ok(sub) => sub,
            Err(err) if is_gone(&err) => return Err(Error::Changed),
            Err(err) if denied(&err) => {
                self.unreadable.push(rel);
                return Ok(());
            }
            Err(err) => return Err(err.into()),
        };
        if !stat_fd(sub.as_fd())?.same_inode(&entry.id) {
            return Err(Error::Changed);
        }
        self.bindings.push((rel.clone(), entry.id));
        self.dir(sub.as_fd(), &rel)
    }

    fn dir(&mut self, dir: BorrowedFd<'_>, rel: &Path) -> Result<()> {
        let result = list(dir).map_err(Error::from).and_then(|names| {
            if self.read_rules(dir, rel, &names) == RulesRead::Unknown {
                self.unreadable.push(rel.to_path_buf());
                return Ok(());
            }
            names
                .iter()
                .filter(|name| !private(rel, name))
                .try_for_each(|name| self.entry(dir, name, rel.join(name)))
        });
        self.unless_denied(rel, result)
    }

    /// Reads the rules file of the folder `dir` at `rel`, which lists `names`,
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
    /// the entries until the search decides. Rescan them later.
    pub unsettled: Vec<PathBuf>,
    /// Folders the scan may not open or list, and folders with a rules file
    /// it cannot read when no earlier scan read one for them, so that what
    /// they leave out is not known. The snapshot keeps what it knew below
    /// them, so a folder the user cannot read never reads as emptied, and
    /// nothing a rules file may leave out is reported as new. Report them,
    /// and rescan them once they can be read.
    pub unreadable: Vec<PathBuf>,
    /// The identity of the root folder the scan opened. A scan that reached
    /// another folder than the caller's root has already applied that folder
    /// to the snapshot, so stop using the snapshot, not only its changes.
    pub root: FileId,
    /// The folders whose rules file this scan found changed, each with the
    /// bytes it holds now, or `None` for a folder that has none any more.
    /// Empty for a scan without [`Rules`].
    pub rules: Vec<(PathBuf, Option<Vec<u8>>)>,
    /// The folders whose rules this scan established, at the paths it listed
    /// them: it read their rules file, found none, or could not read one and
    /// kept the rules an earlier scan read. A folder reported unreadable
    /// before has known rules again once it is here, whatever its rules file
    /// did meanwhile. Empty for a scan without [`Rules`].
    pub read: Vec<PathBuf>,
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
    /// In each folder it lists, the scan reads the rules file before it judges
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
    /// the search has decided it, or as a rename when that rescan finds its
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
            None,
            Some(rules),
            &|| false,
            &mut |_| {},
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
        for path in held {
            self.entries.remove(&path);
        }
        self.left_out.retain(|path, _| !path.starts_with(scope));
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
                        executable: false,
                        linked: kind == EntryKind::File,
                    };
                    (path, entry)
                }),
        );
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
        self.rescan_under(root, paths, expected, None, &|| false, cut)
    }

    /// [`Snapshot::rescan_with`], leaving out what `rules` leave out, and
    /// failing with [`Error::Stopped`] once `stopped` holds. A scan that fails
    /// takes back what it changed in `rules`.
    fn rescan_under(
        &mut self,
        root: &Path,
        paths: impl IntoIterator<Item = PathBuf>,
        expected: Option<FileId>,
        rules: Option<&mut Rules>,
        stopped: &dyn Fn() -> bool,
        cut: &mut dyn FnMut(Cut<'_>),
    ) -> Result<Rescan> {
        let mut scan = Scan {
            new: BTreeMap::new(),
            bindings: Vec::new(),
            replaced: Vec::new(),
            unreadable: Vec::new(),
            left_out: BTreeMap::new(),
            rules: rules.map(Pending::new),
            cut,
            stopped,
        };
        let result = self.rescan_into(root, paths, expected, &mut scan);
        if result.is_err()
            && let Some(rules) = scan.rules
        {
            rules.undo();
        }
        result
    }

    /// Scans the given paths into `scan` and applies what it found to the
    /// snapshot and to the rules of `scan`. Nothing after the last step that
    /// can fail changes either.
    fn rescan_into(
        &mut self,
        root: &Path,
        paths: impl IntoIterator<Item = PathBuf>,
        expected: Option<FileId>,
        scan: &mut Scan<'_>,
    ) -> Result<Rescan> {
        let root_dir = open_root(root)?;
        let root_stat = stat_fd(root_dir.as_fd())?;
        if expected.is_some_and(|expected| root_stat.id != expected) {
            return Err(Error::RootReplaced);
        }
        (scan.cut)(Cut::RootOpened);

        let mut scopes = Vec::new();
        for path in paths {
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

        let scopes = outermost(scopes);
        if scopes
            .first()
            .is_some_and(|scope| scope.as_os_str().is_empty())
        {
            scan.replaced.push(PathBuf::new());
            scan.dir(root_dir.as_fd(), Path::new(""))?;
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
            for scope in outermost(resolved) {
                let name = scope
                    .file_name()
                    .expect("scopes are non-empty")
                    .to_os_string();
                let parent = scope.parent().unwrap_or(Path::new("")).to_path_buf();
                groups.entry(parent).or_default().insert(name);
            }
            for (parent, focus) in groups {
                let result = self.scan_children(scan, &root_dir, &parent, &focus);
                scan.unless_denied(&parent, result)?;
            }
        }

        let again = open_root(root)?;
        if !stat_fd(again.as_fd())?.same_inode(&root_stat.id) {
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
        let unreadable = std::mem::take(&mut scan.unreadable);
        for blocked in &unreadable {
            scan.new
                .retain(|path, _| path == blocked || !path.starts_with(blocked));
            let known = scan
                .new
                .get(blocked)
                .and_then(|now| {
                    old.iter()
                        .find(|(_, before)| before.kind == EntryKind::Dir && before.id == now.id)
                })
                .map_or(blocked, |(path, _)| path)
                .clone();
            let below: Vec<(PathBuf, Entry)> = (old.range(known.clone()..))
                .take_while(|(path, _)| path.starts_with(&known))
                .filter(|(path, _)| **path != known)
                .map(|(path, entry)| (path.clone(), *entry))
                .collect();
            for (path, entry) in below {
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
            &root_stat.id,
            again.as_fd(),
            &old,
            &mut scan.new,
            &mut *scan.cut,
        )?;
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
                    &root_stat.id,
                    again.as_fd(),
                    &held,
                    &mut there,
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
        let mut changes = diff(&old, &scan.new);
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
                held,
                &earlier,
                rules,
                again.as_fd(),
                &mut *scan.cut,
            );
            let arrivals: BTreeSet<PathBuf> = arrivals.into_iter().collect();
            scan.new
                .retain(|path, _| !path.ancestors().any(|above| arrivals.contains(above)));
            unsettled.extend(postponed.keys().cloned());
            scan.new.extend(postponed);
            unsettled.extend(outermost(rules.held().cloned().collect()));
            unsettled.sort();
        }
        for path in old.keys().chain(&left_out) {
            self.entries.remove(path);
        }
        self.entries.append(&mut scan.new);
        for path in &was_left_out {
            self.left_out.remove(path);
        }
        self.left_out.extend(still_left_out);
        self.left_out.extend(changes.iter().filter_map(|change| {
            match change {
                Change::Ignored { to, kind, id, .. } => (scan.left_out.get(to))
                    .filter(|now| now.kind == *kind && now.id == *id)
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
        Ok(Rescan {
            changes,
            unsettled,
            unreadable,
            root: root_stat.id,
            rules,
            read,
        })
    }

    /// Rescans the children of the known folder `parent`: every name in
    /// `focus`, every listed name the snapshot does not know, and every known
    /// child the listing no longer contains. When the folder's rules file
    /// changed, it rescans every child, because the change can include or
    /// leave out anything below the folder.
    fn scan_children(
        &self,
        scan: &mut Scan<'_>,
        root: &OwnedFd,
        parent: &Path,
        focus: &BTreeSet<OsString>,
    ) -> Result<()> {
        let dir = if parent.as_os_str().is_empty() {
            root.try_clone()?
        } else {
            let (dir, chain) = self.open_known_dir(root, parent)?.ok_or(Error::Changed)?;
            scan.bindings.extend(chain);
            dir
        };
        let dir = dir.as_fd();
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
                scan.entry(dir, name, child)?;
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
    ) -> Result<Option<(OwnedFd, Vec<(PathBuf, FileId)>)>> {
        let Some(known) = self.entries.get(rel).filter(|e| e.kind == EntryKind::Dir) else {
            return Ok(None);
        };
        let mut dir = root.try_clone()?;
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
            chain.push((path.clone(), stat_fd(dir.as_fd())?.id));
        }
        let same = chain
            .last()
            .is_some_and(|(_, id)| id.dev == known.id.dev && id.ino == known.id.ino);
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

/// Checks that every folder the scan opened is still the folder at its path,
/// opening each again from the root without following links. A folder that
/// was moved, replaced or made unreadable after the scan opened it means the
/// scan may have read a detached copy, so nothing it saw is published.
fn revalidate(root: BorrowedFd<'_>, mut bindings: Vec<(PathBuf, FileId)>) -> Result<()> {
    bindings.sort_by(|a, b| a.0.cmp(&b.0));
    bindings.dedup_by(|a, b| a.0 == b.0);
    let mut open: Vec<(PathBuf, OwnedFd)> = Vec::new();
    for (path, id) in bindings {
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
        if !stat_fd(dir.as_fd()).is_ok_and(|stat| stat.same_inode(&id)) {
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
/// final look from a freshly opened root sees that parent unchanged.
fn keep_unconfirmed(
    root: &Path,
    root_id: &FileId,
    attached: BorrowedFd<'_>,
    old: &BTreeMap<PathBuf, Entry>,
    new: &mut BTreeMap<PathBuf, Entry>,
    cut: &mut dyn FnMut(Cut<'_>),
) -> Result<Vec<PathBuf>> {
    let gone: Vec<(PathBuf, EntryKind)> = old
        .iter()
        .filter(|(path, before)| {
            !new.get(*path)
                .is_some_and(|after| after.kind == before.kind)
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
    if !stat_fd(fresh.as_fd())?.same_inode(root_id) {
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
            let (top, below) = (roots.iter())
                .find_map(|top| Some((top, path.strip_prefix(top).ok()?)))
                .expect("held below a root");
            let mut name = top.as_os_str().to_owned();
            name.push("\0");
            Path::new(&name)
                .components()
                .chain(below.components())
                .collect()
        };
        merged = (old.iter())
            .map(|(path, entry)| (path.clone(), *entry))
            .chain(held.iter().map(|(path, entry)| (aside(path), *entry)))
            .collect();
        *changes = diff(&merged, new);
        &merged
    };
    let (arrivals, postponed) = hold_back(changes, old, new, earlier, rules, root, cut);
    for change in changes.iter_mut() {
        match change {
            Change::Removed { path, .. }
            | Change::Renamed { from: path, .. }
            | Change::Ignored { from: path, .. } => *path = unaside(path),
            Change::Created { .. } | Change::Modified { .. } | Change::Reidentified { .. } => {}
        }
    }
    let created: BTreeSet<&PathBuf> = (changes.iter())
        .filter_map(|change| match change {
            Change::Created {
                path,
                kind: EntryKind::File,
            } => Some(path),
            _ => None,
        })
        .collect();
    let saved: BTreeSet<PathBuf> = (changes.iter())
        .filter_map(|change| match change {
            Change::Removed {
                path,
                kind: EntryKind::File,
            } if created.contains(path) => Some(path.clone()),
            _ => None,
        })
        .collect();
    changes
        .retain(|change| !matches!(change, Change::Created { path, .. } if saved.contains(path)));
    for change in changes.iter_mut() {
        if let Change::Removed { path, .. } = change
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
    let replaced: BTreeSet<PathBuf> = (changes.iter())
        .filter_map(|change| match change {
            Change::Modified { path }
                if (old.get(path).zip(new.get(path)))
                    .is_some_and(|(before, after)| after.id != before.id) =>
            {
                Some(path.clone())
            }
            _ => None,
        })
        .collect();
    let removed: BTreeSet<PathBuf> = (changes.iter())
        .filter_map(|change| match change {
            Change::Removed { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect();
    if removed.is_empty() && replaced.is_empty() {
        rules.hold(BTreeMap::new());
        return (Vec::new(), BTreeMap::new());
    }
    let renamed: HashMap<PathBuf, PathBuf> = (changes.iter())
        .filter_map(|change| match change {
            Change::Renamed { from, to, .. } => Some((from.clone(), to.clone())),
            _ => None,
        })
        .collect();
    let carried = |path: &Path| {
        let entry = &old[path];
        (path.ancestors())
            .filter_map(|from| Some((renamed.get(from)?, path.strip_prefix(from).ok()?)))
            .any(|(to, below)| {
                (new.get(&to.join(below)))
                    .is_some_and(|after| after.id == entry.id && after.kind == entry.kind)
            })
    };
    let mut held: BTreeSet<&PathBuf> = (removed.iter())
        .filter(|dir| old[*dir].kind == EntryKind::Dir)
        .flat_map(|dir| {
            (old.range(dir.clone()..).skip(1))
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
                && (held.iter().copied())
                    .filter(|inside| inside.starts_with(path))
                    .chain([*path])
                    .any(|inside| undecided.contains(&key(inside)))
        })
        .map(|path| (path.clone(), relocated(path)))
        .collect();
    let taken = |path: &Path| (deferred.iter()).any(|(_, to)| path.starts_with(to));
    let mut arrivals: Vec<PathBuf> = (changes.iter())
        .filter_map(|change| match change {
            Change::Created { path, .. } if taken(path) => Some(path.clone()),
            _ => None,
        })
        .collect();
    let finish = (changes.iter()).any(|change| match change {
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
        Change::Created { path, .. } => !arrivals.contains(path),
        Change::Modified { path } => !unseen.contains(path) && !postponed.contains_key(path),
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
    let mut moved: Vec<Change> = (held.iter())
        .filter(|path| {
            !(path.ancestors().skip(1))
                .take_while(|above| old.contains_key(*above))
                .any(|above| found(above).is_some())
        })
        .filter_map(|path| found(path))
        .collect();
    for change in changes.iter_mut() {
        match &*change {
            Change::Removed { path, .. } if !exempt.contains(&path) => {
                if let Some(ignored) = found(path) {
                    *change = ignored;
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

fn diff(old: &BTreeMap<PathBuf, Entry>, new: &BTreeMap<PathBuf, Entry>) -> Vec<Change> {
    let moved: HashSet<FileId> = old
        .iter()
        .filter(|(path, before)| {
            before.kind == EntryKind::Dir
                && !new
                    .get(*path)
                    .is_some_and(|after| after.kind == EntryKind::Dir)
        })
        .map(|(_, before)| before.id)
        .collect();
    let dirs: HashSet<FileId> = (new.values())
        .filter(|after| after.kind == EntryKind::Dir)
        .map(|after| after.id)
        .collect();
    let ids = |tree: &BTreeMap<PathBuf, Entry>| -> HashSet<(FileId, EntryKind)> {
        tree.values().map(|entry| (entry.id, entry.kind)).collect()
    };
    let (ids_before, ids_after) = (ids(old), ids(new));
    let kept = |path: &Path, before: &Entry, after: &Entry| {
        after.kind == before.kind
            && !(after.id != before.id
                && ids_before.contains(&(after.id, after.kind))
                && ids_after.contains(&(before.id, before.kind)))
            && !(after.kind == EntryKind::Dir
                && after.id != before.id
                && dirs.contains(&before.id)
                && !(old.range(path.to_path_buf()..).skip(1))
                    .take_while(|(inside, _)| inside.starts_with(path))
                    .any(|(inside, entry)| {
                        (new.get(inside))
                            .is_some_and(|after| (after.id, after.kind) == (entry.id, entry.kind))
                    }))
            && !(after.kind == EntryKind::Dir
                && after.id != before.id
                && moved.contains(&after.id)
                && (dirs.contains(&before.id)
                    || !old
                        .range(path.to_path_buf()..)
                        .nth(1)
                        .is_some_and(|(inside, _)| inside.starts_with(path))))
    };
    let mut changed: HashSet<&Path> = HashSet::new();
    for (path, before) in old {
        let parent_changed = path.parent().is_some_and(|parent| changed.contains(parent));
        if parent_changed || !new.get(path).is_some_and(|after| kept(path, before, after)) {
            changed.insert(path);
        }
    }
    let mut removed: BTreeMap<&Path, &Entry> = BTreeMap::new();
    let mut created: Vec<(&Path, &Entry)> = Vec::new();
    let mut modified: Vec<&Path> = Vec::new();
    let mut reidentified: Vec<&Path> = Vec::new();
    for (path, before) in old {
        match new.get(path) {
            Some(after) if !changed.contains(path.as_path()) => {
                if after.kind == EntryKind::File && after != before {
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
    for (path, after) in new {
        if !old.contains_key(path) || changed.contains(path.as_path()) {
            created.push((path, after));
        }
    }

    let mut by_id: HashMap<(FileId, EntryKind), VecDeque<&Path>> = HashMap::new();
    for (path, entry) in &removed {
        by_id
            .entry((entry.id, entry.kind))
            .or_default()
            .push_back(path);
    }
    let mut renames: Vec<(&Path, &Path, &Entry)> = Vec::new();
    created.retain(|&(to, after)| {
        let Some(from) = by_id
            .get_mut(&(after.id, after.kind))
            .and_then(VecDeque::pop_front)
        else {
            return true;
        };
        let before = removed.remove(from).expect("indexed from removed");
        if after.kind == EntryKind::File && after != before {
            modified.push(to);
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
    /// change under it. The next poll rescans them.
    pub pending: Vec<PathBuf>,
    /// Folders the scans of this poll may not read; see
    /// [`Rescan::unreadable`].
    pub unreadable: Vec<PathBuf>,
    /// The identity of the root folder the scans of this poll opened, or
    /// `None` when the poll scanned nothing; see [`Rescan::root`].
    pub root: Option<FileId>,
    /// The folders whose rules file the scans of this poll found changed, in
    /// the order they found them; see [`Rescan::rules`].
    pub rules: Vec<(PathBuf, Option<Vec<u8>>)>,
    /// The folders whose rules the scans of this poll established and that no
    /// later scan of the poll found unreadable; see [`Rescan::read`].
    pub read: Vec<PathBuf>,
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
    unreadable: BTreeSet<PathBuf>,
    rules: Option<Rules>,
    /// The rules changes of the scans since the last poll returned.
    changed_rules: Vec<(PathBuf, Option<Vec<u8>>)>,
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
    failed: Option<Error>,
    stopped: Box<dyn Fn() -> bool + Send>,
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
            root,
            snapshot: previous,
            view: OnceCell::new(),
            dirty: BTreeSet::from([PathBuf::new()]),
            unreadable: BTreeSet::new(),
            #[cfg(target_os = "linux")]
            blocked: BTreeSet::from_iter(deferred.then(PathBuf::new)),
            uncovered: None,
            rules,
            changed_rules: Vec::new(),
            read: BTreeSet::new(),
            rewatch: false,
            failed: None,
            stopped: Box::new(|| false),
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

    fn note(&mut self, event: notify::Result<notify::Event>) -> Result<()> {
        let event = event.map_err(watch_error)?;
        if event.need_rescan() {
            self.rewatch = true;
            self.dirty.insert(PathBuf::new());
        }
        if let EventKind::Access(access) = event.kind
            && access != AccessKind::Close(AccessMode::Write)
        {
            return Ok(());
        }
        for path in event.paths {
            let rel = path.strip_prefix(&self.root).unwrap_or(Path::new(""));
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
            pending: self.dirty.iter().cloned().collect(),
            unreadable: std::mem::take(&mut self.unreadable).into_iter().collect(),
            root,
            rules: std::mem::take(&mut self.changed_rules),
            read: std::mem::take(&mut self.read).into_iter().collect(),
        }
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
            if self.dirty.is_empty() && self.uncovered.is_none() {
                let left = deadline.saturating_duration_since(Instant::now());
                if !self.receive(left)? {
                    return Ok(self.report(Vec::new(), root));
                }
            }
            let settle_until = Instant::now() + MAX_SETTLE;
            while Instant::now() < settle_until && self.receive(SETTLE)? {}
            if self.rules.is_some() && self.dirty.is_empty() && self.uncovered.is_none() {
                // Every event of the batch lay below a left-out folder. Such
                // events can keep coming, so they must not hold the poll
                // past its deadline.
                if Instant::now() >= deadline {
                    return Ok(self.report(Vec::new(), root));
                }
                continue;
            }
            if self.rewatch {
                let deferred = cfg!(target_os = "linux") && self.rules.is_some();
                self.watcher = watch_root(&self.root, &self.sender, deferred)?;
                #[cfg(target_os = "linux")]
                self.blocked.extend(deferred.then(PathBuf::new));
                self.rewatch = false;
            }
            let (expected, uncovered) = self.uncovered.clone().unzip();
            let scopes: Vec<PathBuf> = (self.dirty.iter())
                .chain(uncovered.iter().flatten())
                .cloned()
                .collect();
            let rescan = match self.snapshot.rescan_under(
                &self.root,
                scopes.iter().cloned(),
                expected,
                self.rules.as_mut(),
                &*self.stopped,
                &mut |_| {},
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
            root = Some(rescan.root);
            self.changed_rules.extend(rescan.rules);
            self.dirty = rescan.unsettled.into_iter().collect();
            #[cfg(target_os = "linux")]
            let readable = self.unblock(&scopes, &rescan.unreadable);
            self.learn(rescan.read, &rescan.unreadable);
            let changes = rescan.changes;
            #[cfg(target_os = "linux")]
            self.cover(&changes, readable, rescan.root);
            if !changes.is_empty() || !self.changed_rules.is_empty() || Instant::now() >= deadline {
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
}

impl RescanWatcher {
    pub fn new(root: impl AsRef<Path>, previous: Snapshot, interval: Duration) -> Result<Self> {
        Ok(Self {
            root: std::fs::canonicalize(root)?,
            snapshot: previous,
            view: OnceCell::new(),
            interval,
            rules: None,
            stopped: Box::new(|| false),
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
}

impl Watcher for RescanWatcher {
    fn poll(&mut self, timeout: Duration) -> Result<Poll> {
        self.view.take();
        let deadline = Instant::now() + timeout;
        loop {
            let rescan = self.snapshot.rescan_under(
                &self.root,
                [PathBuf::new()],
                None,
                self.rules.as_mut(),
                &*self.stopped,
                &mut |_| {},
            );
            let poll = match rescan {
                Ok(rescan) => Poll {
                    changes: rescan.changes,
                    pending: rescan.unsettled,
                    unreadable: rescan.unreadable,
                    root: Some(rescan.root),
                    rules: rescan.rules,
                    read: rescan.read,
                },
                Err(Error::Changed) => Poll {
                    pending: vec![PathBuf::new()],
                    ..Poll::default()
                },
                Err(err) => return Err(err),
            };
            let left = deadline.saturating_duration_since(Instant::now());
            if !poll.changes.is_empty() || !poll.rules.is_empty() || left.is_zero() {
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

mod effect;
mod ignore;
mod recovered;
mod resolve;
mod root;
#[cfg(test)]
mod tests;
mod trash;

pub use effect::{
    Action, Contents, Displaced, Effect, Outcome, QUARANTINE_DIR, Reason, Refusal, STAGING_DIR,
    Section, Step,
};
pub use ignore::{Ignore, Ignored, RULES_FILE, Rules};
use ignore::{Key, Pending, RulesRead, Seeded, Wanted};
pub use recovered::{recovered_name, utc_date};
pub use resolve::{Base, Dir, Folder, Location, PARKED_DIR, Space, Unresolved};
pub use root::{
    Marker, PRIVATE_DIR, RootProblem, check_root, probe_no_replace, read_marker, rename_no_replace,
    write_marker,
};
pub use trash::{Reclaimed, TRASH_DIR, TRASH_RETENTION};
