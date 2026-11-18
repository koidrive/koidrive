// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;

use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};

use crate::resolve::Space;
use crate::{FileId, list, open_dir_at, open_root, stat_at, stat_fd};

/// The engine's private folder at the root of a space, for the marker,
/// staging, quarantine, parked entries and the local trash. Scans and watchers
/// never report anything in it.
pub const PRIVATE_DIR: &str = ".koidrive";

const MARKER: &str = "space.json";
const MARKER_TEMP: &str = "space.json.tmp";
const PROBE: [&str; 3] = [
    "no-replace-probe-a",
    "no-replace-probe-b",
    "no-replace-probe-c",
];

/// The root marker `.koidrive/space.json`: the space the folder holds, the
/// effect counter *m* of design §2.8, and the token of the folder's latest
/// start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    pub space_id: String,
    pub effects: u64,
    /// A random token each start of the folder's sync replaces, so a byte
    /// copy of the folder made before that start carries an older one.
    /// `None` in a marker from a build without tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

/// A new marker token: 128 random bits in hex.
pub fn new_token() -> String {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).expect("the operating system provides random bytes");
    format!("{:032x}", u128::from_be_bytes(bytes))
}

/// The marker tokens a link records: the one the marker carries since the
/// latest start, and the one a start is writing to it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RootTokens {
    pub current: Option<String>,
    pub pending: Option<String>,
}

impl RootTokens {
    /// Whether `marker` carries a token this link wrote at its latest start
    /// or is writing now. A link that records no token yet admits any marker.
    pub fn admits(&self, marker: &Marker) -> Result<(), RootProblem> {
        let Some(current) = &self.current else {
            return Ok(());
        };
        match &marker.token {
            Some(token) if token == current || Some(token) == self.pending.as_ref() => Ok(()),
            _ => Err(RootProblem::Copy),
        }
    }
}

/// Why a root fails the check of design §6.2. No operation may be built from a
/// scan of a root that fails it.
#[derive(Debug, thiserror::Error)]
pub enum RootProblem {
    #[error("the root folder is missing")]
    Missing,
    #[error("the root is not a folder")]
    NotAFolder,
    #[error("the root folder cannot be read: {0}")]
    Unreadable(io::Error),
    #[error(
        "the disk that holds the folder was mounted again under another number, and Koidrive can't confirm it is the same disk"
    )]
    Renumbered,
    #[error("the folder is on another disk than the one it was linked on")]
    OtherDisk,
    #[error("the root folder has no marker")]
    NoMarker,
    #[error("the root marker cannot be read: {0}")]
    BadMarker(io::Error),
    #[error("the root folder belongs to space {0}")]
    OtherSpace(String),
    #[error(
        "the folder at this path is an older copy of the linked folder, so sync stopped before files missing from the copy were taken as deleted"
    )]
    Copy,
}

/// What names a volume under any device number, such as after an OS update:
/// its UUID, on btrfs the UUID of the subvolume, or on ext2, ext3 and ext4 the
/// file system ID that Linux derives from the UUID. The kinds have different
/// lengths, so [`as_bytes`](Self::as_bytes) names its kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VolumeId {
    Uuid([u8; 16]),
    FileSystemId([u8; 8]),
}

impl VolumeId {
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Uuid(uuid) => uuid,
            Self::FileSystemId(id) => id,
        }
    }

    /// The id that [`as_bytes`](Self::as_bytes) gave, or `None` for any other
    /// length.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        match bytes.len() {
            16 => bytes.try_into().ok().map(Self::Uuid),
            8 => bytes.try_into().ok().map(Self::FileSystemId),
            _ => None,
        }
    }
}

/// A root as a link records it or as it is found: the device number it is
/// mounted under, and the UUID of its volume where the volume has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootAt {
    pub dev: u64,
    pub volume: Option<VolumeId>,
}

impl RootAt {
    /// Whether `found` is on the volume this link recorded. A volume with a
    /// UUID is the same volume under any device number. Without one, only the
    /// device number tells volumes apart.
    pub fn admits(&self, found: &RootAt) -> Result<(), RootProblem> {
        match (self.volume, found.volume) {
            (Some(linked), Some(found)) if linked == found => Ok(()),
            (Some(_), _) => Err(RootProblem::OtherDisk),
            (None, _) if found.dev == self.dev => Ok(()),
            (None, _) => Err(RootProblem::Renumbered),
        }
    }
}

/// The root folder `dir` as it is found now.
pub fn root_at(dir: BorrowedFd<'_>) -> io::Result<RootAt> {
    let stat = stat_fd(dir)?;
    Ok(RootAt {
        dev: stat.dev,
        volume: volume_id(dir),
    })
}

#[cfg(target_vendor = "apple")]
pub(crate) fn volume_id(dir: BorrowedFd<'_>) -> Option<VolumeId> {
    use std::os::fd::AsRawFd;
    #[repr(C, packed(4))]
    struct Reply {
        length: u32,
        uuid: [u8; 16],
    }
    let mut request = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: libc::ATTR_VOL_INFO | libc::ATTR_VOL_UUID,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    let mut reply = Reply {
        length: 0,
        uuid: [0; 16],
    };
    let size = std::mem::size_of::<Reply>();
    let done = unsafe {
        libc::fgetattrlist(
            dir.as_raw_fd(),
            (&raw mut request).cast(),
            (&raw mut reply).cast(),
            size,
            0,
        )
    };
    let uuid = reply.uuid;
    (done == 0 && reply.length as usize == size && uuid != [0; 16]).then_some(VolumeId::Uuid(uuid))
}

#[cfg(target_os = "linux")]
pub(crate) fn volume_id(dir: BorrowedFd<'_>) -> Option<VolumeId> {
    btrfs_subvolume_id(dir)
        .or_else(|| ext_file_system_id(dir))
        .or_else(|| file_system_id(dir))
}

#[cfg(target_os = "linux")]
fn ext_file_system_id(dir: BorrowedFd<'_>) -> Option<VolumeId> {
    const EXT_SUPER_MAGIC: u64 = 0xEF53;
    let stat = rustix::fs::fstatfs(dir).ok()?;
    let id: [u8; 8] = unsafe { std::mem::transmute(stat.f_fsid) };
    (stat.f_type as u64 == EXT_SUPER_MAGIC && id != [0; 8]).then_some(VolumeId::FileSystemId(id))
}

#[cfg(target_os = "linux")]
fn btrfs_subvolume_id(dir: BorrowedFd<'_>) -> Option<VolumeId> {
    use std::os::fd::AsRawFd;
    #[repr(C)]
    struct SubvolInfo {
        before: [u64; 37],
        uuid: [u8; 16],
        after: [u8; 192],
    }
    const BTRFS_IOC_GET_SUBVOL_INFO: libc::c_ulong =
        0x8000_943c | (std::mem::size_of::<SubvolInfo>() as libc::c_ulong) << 16;
    let mut reply = SubvolInfo {
        before: [0; 37],
        uuid: [0; 16],
        after: [0; 192],
    };
    let done = unsafe {
        libc::ioctl(
            dir.as_raw_fd(),
            BTRFS_IOC_GET_SUBVOL_INFO as _,
            &raw mut reply,
        )
    };
    (done >= 0 && reply.uuid != [0; 16]).then_some(VolumeId::Uuid(reply.uuid))
}

#[cfg(target_os = "linux")]
fn file_system_id(dir: BorrowedFd<'_>) -> Option<VolumeId> {
    use std::os::fd::AsRawFd;
    #[repr(C)]
    struct FsUuid2 {
        len: u8,
        uuid: [u8; 16],
    }
    const FS_IOC_GETFSUUID: libc::c_ulong =
        0x8000_1500 | (std::mem::size_of::<FsUuid2>() as libc::c_ulong) << 16;
    let mut reply = FsUuid2 {
        len: 0,
        uuid: [0; 16],
    };
    let done = unsafe { libc::ioctl(dir.as_raw_fd(), FS_IOC_GETFSUUID as _, &raw mut reply) };
    (done == 0 && reply.len == 16 && reply.uuid != [0; 16]).then_some(VolumeId::Uuid(reply.uuid))
}

#[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
pub(crate) fn volume_id(_: BorrowedFd<'_>) -> Option<VolumeId> {
    None
}

/// Checks the root of space `space_id` before a scan that can report removals:
/// it exists, is a folder that can be listed, is the root `linked` admits, and
/// its marker names the space. Returns the marker.
///
/// The scan itself must also come back without unreadable folders
/// ([`Rescan::unreadable`](crate::Rescan::unreadable)) before its removals are
/// acted on.
pub fn check_root(root: &Path, space_id: &str, linked: &RootAt) -> Result<Marker, RootProblem> {
    check_root_at(open_root_folder(root)?.as_fd(), space_id, linked)
}

/// Opens the root folder at `root` for [`check_root_at`].
pub fn open_root_folder(root: &Path) -> Result<OwnedFd, RootProblem> {
    open_root(root).map_err(|err| match err.kind() {
        io::ErrorKind::NotFound => RootProblem::Missing,
        io::ErrorKind::NotADirectory => RootProblem::NotAFolder,
        _ => RootProblem::Unreadable(err),
    })
}

/// [`check_root`] of the root folder `dir` holds, whatever its path names now.
pub fn check_root_at(
    dir: BorrowedFd<'_>,
    space_id: &str,
    linked: &RootAt,
) -> Result<Marker, RootProblem> {
    let found = root_at(dir).map_err(RootProblem::Unreadable)?;
    list(dir).map_err(RootProblem::Unreadable)?;
    linked.admits(&found)?;
    let marker = read_marker_at(dir)
        .map_err(RootProblem::BadMarker)?
        .ok_or(RootProblem::NoMarker)?;
    if marker.space_id != space_id {
        return Err(RootProblem::OtherSpace(marker.space_id));
    }
    Ok(marker)
}

/// Reads the root marker, or returns `None` if there is none.
pub fn read_marker(root: &Path) -> io::Result<Option<Marker>> {
    read_marker_at(open_root(root)?.as_fd())
}

fn read_marker_at(root: BorrowedFd<'_>) -> io::Result<Option<Marker>> {
    let private = match open_dir_at(root, OsStr::new(PRIVATE_DIR)) {
        Ok(private) => private,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let file = match rustix::fs::openat(&private, MARKER, flags, Mode::empty()) {
        Ok(file) => file,
        Err(Errno::NOENT) => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let mut text = String::new();
    File::from(file).read_to_string(&mut text)?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

/// Writes the root marker and flushes it, with the private folder that holds
/// it, before returning. The marker is replaced in one rename, so a crash
/// leaves the old marker or the new one.
pub fn write_marker(root: &Path, marker: &Marker) -> io::Result<()> {
    write_marker_at(open_root(root)?.as_fd(), marker, Flush::Full)
}

pub(crate) fn write_marker_at(
    root: BorrowedFd<'_>,
    marker: &Marker,
    then: Flush,
) -> io::Result<()> {
    let private = private_dir(root)?;
    replace_file(
        private.as_fd(),
        MARKER,
        MARKER_TEMP,
        &serde_json::to_vec_pretty(marker)?,
        then,
    )
}

pub(crate) fn replace_file(
    dir: BorrowedFd<'_>,
    name: &str,
    temp: &str,
    bytes: &[u8],
    then: Flush,
) -> io::Result<()> {
    remove_if_present(dir, temp)?;
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut file = File::from(rustix::fs::openat(
        dir,
        temp,
        flags,
        Mode::from_raw_mode(0o644),
    )?);
    file.write_all(bytes)?;
    flush(file.as_fd(), Flush::Barrier)?;
    rustix::fs::renameat(dir, temp, dir, name)?;
    flush(dir, then)
}

/// Whether the file system under `root` has a rename that fails instead of
/// replacing its target. Effects need it, so a space on a file system without
/// it is refused (design §5.2). The probe works on files of its own in the
/// private folder, which it creates if needed, and removes them again. It
/// requires both halves: a no-replace rename onto an existing file fails with
/// `EEXIST` and leaves both files where they were, and one onto a free name
/// moves the file. Linux refuses the first before the file system sees the
/// flag, so only the second shows a file system that does not support it.
pub fn probe_no_replace(root: &Path) -> io::Result<bool> {
    probe_in(private_dir(open_root(root)?.as_fd())?.as_fd())
}

impl Space {
    /// Reads the root marker through the root descriptor, or returns `None`
    /// if there is none. It writes nothing, so the caller can read the counter
    /// before [`Space::section`] writes the marker again. It reads the root
    /// the space holds whether or not that root is still held.
    pub fn marker(&self) -> io::Result<Option<Marker>> {
        read_marker_at(self.root())
    }

    /// The root the space holds, as it is found now.
    pub fn root_at(&self) -> io::Result<RootAt> {
        root_at(self.root())
    }

    /// The identity of the entry `name` directly in the root the space holds,
    /// or `None` when there is none.
    pub fn child_id(&self, name: &OsStr) -> io::Result<Option<FileId>> {
        match stat_at(self.root(), name) {
            Ok(stat) => Ok(Some(stat.id)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// [`probe_no_replace`] in the private folder of the root the space
    /// holds. Fails, creating nothing, unless the root is held.
    pub fn probe_no_replace(&self) -> io::Result<bool> {
        probe_in(self.private(&[])?.fd())
    }

    /// [`write_marker`] through the root the space holds. Fails, writing
    /// nothing, unless the root is held.
    pub fn write_marker(&self, marker: &Marker) -> io::Result<()> {
        self.check_held()?;
        write_marker_at(self.root(), marker, Flush::Full)
    }
}

fn probe_in(private: BorrowedFd<'_>) -> io::Result<bool> {
    let [a, b, _] = PROBE;
    for name in PROBE {
        remove_if_present(private, name)?;
    }
    for name in [a, b] {
        let flags =
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        rustix::fs::openat(private, name, flags, Mode::from_raw_mode(0o600))?;
    }
    let supported = probe_renames(private)?;
    for name in PROBE {
        remove_if_present(private, name)?;
    }
    Ok(supported)
}

fn probe_renames(private: BorrowedFd<'_>) -> io::Result<bool> {
    let [a, b, c] = PROBE.map(OsStr::new);
    let (id_a, id_b) = (probe_id(private, a)?, probe_id(private, b)?);
    match rename_no_replace(private, a, private, b) {
        Err(err) if err.raw_os_error() == Some(Errno::EXIST.raw_os_error()) => {}
        Err(err) if !unsupported(&err) => return Err(err),
        _ => return Ok(false),
    }
    if probe_id(private, a)? != id_a || probe_id(private, b)? != id_b {
        return Ok(false);
    }
    match rename_no_replace(private, a, private, c) {
        Ok(()) => {}
        Err(err) if unsupported(&err) => return Ok(false),
        Err(err) => return Err(err),
    }
    Ok(probe_id(private, a)?.is_none() && probe_id(private, c)? == id_a)
}

fn probe_id(private: BorrowedFd<'_>, name: &OsStr) -> io::Result<Option<FileId>> {
    match stat_at(private, name) {
        Ok(stat) => Ok(Some(stat.id)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

pub fn rename_no_replace(
    from_dir: BorrowedFd<'_>,
    from: &OsStr,
    to_dir: BorrowedFd<'_>,
    to: &OsStr,
) -> io::Result<()> {
    Ok(rustix::fs::renameat_with(
        from_dir,
        from,
        to_dir,
        to,
        RenameFlags::NOREPLACE,
    )?)
}

fn unsupported(err: &io::Error) -> bool {
    [Errno::INVAL, Errno::NOSYS, Errno::NOTSUP, Errno::OPNOTSUPP]
        .iter()
        .any(|errno| err.raw_os_error() == Some(errno.raw_os_error()))
}

pub(crate) fn private_dir(root: BorrowedFd<'_>) -> io::Result<OwnedFd> {
    make_dir(root, PRIVATE_DIR)
}

pub(crate) fn make_dir(dir: BorrowedFd<'_>, name: &str) -> io::Result<OwnedFd> {
    match rustix::fs::mkdirat(dir, name, Mode::from_raw_mode(0o755)) {
        Ok(()) => flush(dir, Flush::Full)?,
        Err(Errno::EXIST) => {}
        Err(err) => return Err(err.into()),
    }
    open_dir_at(dir, OsStr::new(name))
}

pub(crate) fn remove_if_present(dir: BorrowedFd<'_>, name: &str) -> io::Result<()> {
    match rustix::fs::unlinkat(dir, name, AtFlags::empty()) {
        Ok(()) | Err(Errno::NOENT) => Ok(()),
        Err(err) => Err(err.into()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flush {
    Plain,
    Barrier,
    Full,
}

/// On macOS `fsync` hands the data to the drive, which may write it late and
/// out of order, so a barrier uses `F_BARRIERFSYNC` and a full flush
/// `F_FULLFSYNC` there (Apple fcntl(2)). Elsewhere `fsync` is durable.
pub(crate) fn flush(fd: BorrowedFd<'_>, how: Flush) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    match how {
        Flush::Plain => rustix::fs::fsync(fd)?,
        Flush::Barrier => barrier(fd)?,
        Flush::Full => rustix::fs::fcntl_fullfsync(fd)?,
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        let _ = how;
        rustix::fs::fsync(fd)?;
    }
    Ok(())
}

#[cfg(target_vendor = "apple")]
fn barrier(fd: BorrowedFd<'_>) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    match unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_BARRIERFSYNC) } {
        -1 => Err(io::Error::last_os_error()),
        _ => Ok(()),
    }
}
