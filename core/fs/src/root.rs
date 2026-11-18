// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::FileExt;
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
const LINEAGE: &str = "lineage";
const LINEAGE_TEMP: &str = "lineage.tmp";
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

/// A marker token this link generated: 128 random bits, written in hex. Nothing turns a string into one, so a token
/// read from a marker or a store can't pass for one this link minted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Minted(u128);

impl Minted {
    pub fn mint() -> Minted {
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes).expect("the operating system provides random bytes");
        Minted(u128::from_be_bytes(bytes))
    }
}

impl std::fmt::Display for Minted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

/// How a rotation writes the folder's token log: after the marker's token, which this link minted, or as a new log
/// that holds only the new token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rotation {
    Extend,
    Restart,
}

/// The most tokens the folder's token log holds once it has grown past twice as many.
pub const LINEAGE_KEPT: usize = 1 << 16;

/// The folder's token log, `.koidrive/lineage`: the tokens its markers took, oldest first, each written before the
/// marker that carries it. One link's rotations extend it only after a token that link minted.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Lineage(Vec<u128>);

impl Lineage {
    /// Whether the token `marker` carries comes after one of `own` in the log.
    pub fn descends(&self, own: &[Option<&str>], marker: &Marker) -> bool {
        let Some(found) = marker.token.as_deref().and_then(token_bits) else {
            return false;
        };
        let Some(at) = self.0.iter().rposition(|token| *token == found) else {
            return false;
        };
        (own.iter().flatten().filter_map(|token| token_bits(token)))
            .any(|token| self.0[..at].contains(&token))
    }

    fn read(private: BorrowedFd<'_>) -> io::Result<Lineage> {
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let file = match rustix::fs::openat(private, LINEAGE, flags, Mode::empty()) {
            Ok(file) => file,
            Err(Errno::NOENT) => return Ok(Lineage::default()),
            Err(err) => return Err(err.into()),
        };
        let mut bytes = Vec::new();
        File::from(file).read_to_end(&mut bytes)?;
        let (tokens, _) = bytes.as_chunks::<16>();
        Ok(Lineage(
            tokens
                .iter()
                .map(|token| u128::from_be_bytes(*token))
                .collect(),
        ))
    }
}

fn token_bits(token: &str) -> Option<u128> {
    let hex = token.len() == 32 && token.bytes().all(|byte| byte.is_ascii_hexdigit());
    hex.then(|| u128::from_str_radix(token, 16).ok()).flatten()
}

fn write_lineage(
    private: BorrowedFd<'_>,
    marker: &Marker,
    next: Minted,
    rotation: Rotation,
) -> io::Result<()> {
    let alone = next.0.to_be_bytes();
    let (Rotation::Extend, Some(found)) = (rotation, marker.token.as_deref().and_then(token_bits))
    else {
        return replace_file(private, LINEAGE, LINEAGE_TEMP, &alone, Flush::Barrier);
    };
    if append_after(private, found, &alone)? {
        return Ok(());
    }
    let mut tokens = Lineage::read(private)?.0;
    let Some(at) = tokens.iter().rposition(|token| *token == found) else {
        return replace_file(private, LINEAGE, LINEAGE_TEMP, &alone, Flush::Barrier);
    };
    tokens.truncate(at + 1);
    tokens.push(next.0);
    if tokens.len() > 2 * LINEAGE_KEPT {
        tokens.drain(..tokens.len() - LINEAGE_KEPT);
    }
    let bytes: Vec<u8> = tokens
        .iter()
        .flat_map(|token| token.to_be_bytes())
        .collect();
    replace_file(private, LINEAGE, LINEAGE_TEMP, &bytes, Flush::Barrier)
}

fn append_after(private: BorrowedFd<'_>, last: u128, next: &[u8; 16]) -> io::Result<bool> {
    let flags = OFlags::RDWR | OFlags::APPEND | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut file = match rustix::fs::openat(private, LINEAGE, flags, Mode::empty()) {
        Ok(file) => File::from(file),
        Err(Errno::NOENT) => return Ok(false),
        Err(err) => return Err(err.into()),
    };
    let len = file.metadata()?.len();
    if len == 0 || len % 16 != 0 || len >= 32 * LINEAGE_KEPT as u64 {
        return Ok(false);
    }
    let mut tail = [0; 16];
    file.read_exact_at(&mut tail, len - 16)?;
    if u128::from_be_bytes(tail) != last {
        return Ok(false);
    }
    file.write_all(next)?;
    Ok(true)
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
///
/// A volume ID shorter than a UUID, such as the 32-bit serial number of a FAT
/// or exFAT volume, is a `Uuid` that holds the ID's bytes as the volume stores
/// them, then zeros, and the ID's length in the last byte.
///
/// Sticks formatted with one serial number share it, so a FAT or exFAT volume
/// is a name-based `Uuid` (<https://www.rfc-editor.org/rfc/rfc9562#section-5.5>)
/// of that ID, the file system and the volume's size.
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

pub(crate) fn volume_id(dir: BorrowedFd<'_>) -> Option<VolumeId> {
    volume_ids(dir).map(|(id, _)| id)
}

pub(crate) fn volume_ids(dir: BorrowedFd<'_>) -> Option<(VolumeId, VolumeId)> {
    let plain = plain_volume_id(dir)?;
    #[cfg(any(target_vendor = "apple", target_os = "linux"))]
    if let Ok(volume) = rustix::fs::fstatfs(dir)
        && let Some(kind) = fat_kind(&volume)
    {
        let size = volume.f_blocks.saturating_mul(volume.f_bsize as u64);
        return Some((sized_volume_id(plain, kind, size), plain));
    }
    Some((plain, plain))
}

#[cfg(any(target_vendor = "apple", target_os = "linux"))]
fn sized_volume_id(plain: VolumeId, kind: u8, size: u64) -> VolumeId {
    use sha1::{Digest, Sha1};
    const NAMESPACE: [u8; 16] = [
        0x6f, 0x8c, 0x2d, 0x4e, 0x1b, 0x7a, 0x5c, 0x3e, 0x9a, 0x0d, 0x4e, 0x2f, 0x7b, 0x6c, 0x1a,
        0x95,
    ];
    let digest = (Sha1::new()
        .chain_update(NAMESPACE)
        .chain_update(plain.as_bytes()))
    .chain_update([kind])
    .chain_update(size.to_le_bytes())
    .finalize();
    let mut uuid = [0; 16];
    uuid.copy_from_slice(&digest[..16]);
    uuid[6] = (uuid[6] & 0x0f) | 0x50;
    uuid[8] = (uuid[8] & 0x3f) | 0x80;
    VolumeId::Uuid(uuid)
}

#[cfg(target_vendor = "apple")]
fn plain_volume_id(dir: BorrowedFd<'_>) -> Option<VolumeId> {
    use std::os::fd::AsRawFd;
    if let Ok(volume) = rustix::fs::fstatfs(dir)
        && fat_kind(&volume) == Some(1)
    {
        return disk_arbitration_volume_id(&volume);
    }
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

/// The volume UUID Disk Arbitration derives from the serial number of the FAT volume `volume`. macOS 15 reports a
/// new UUID through `getattrlist` each time it mounts a FAT volume again.
#[cfg(target_vendor = "apple")]
fn disk_arbitration_volume_id(volume: &rustix::fs::StatFs) -> Option<VolumeId> {
    use std::ffi::{c_char, c_void};
    type Object = *const c_void;
    #[repr(C)]
    struct UuidBytes([u8; 16]);
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(object: Object);
        fn CFGetTypeID(object: Object) -> usize;
        fn CFUUIDGetTypeID() -> usize;
        fn CFUUIDGetUUIDBytes(uuid: Object) -> UuidBytes;
        fn CFDictionaryGetValue(dictionary: Object, key: Object) -> Object;
    }
    #[link(name = "DiskArbitration", kind = "framework")]
    unsafe extern "C" {
        static kDADiskDescriptionVolumeUUIDKey: Object;
        fn DASessionCreate(allocator: Object) -> Object;
        fn DADiskCreateFromBSDName(
            allocator: Object,
            session: Object,
            name: *const c_char,
        ) -> Object;
        fn DADiskCopyDescription(disk: Object) -> Object;
    }
    let owned = |object: Object| (!object.is_null()).then_some(object);
    let session = owned(unsafe { DASessionCreate(std::ptr::null()) })?;
    let disk = owned(unsafe {
        DADiskCreateFromBSDName(std::ptr::null(), session, volume.f_mntfromname.as_ptr())
    });
    let description = disk.and_then(|disk| owned(unsafe { DADiskCopyDescription(disk) }));
    let uuid = description.and_then(|description| unsafe {
        let uuid = CFDictionaryGetValue(description, kDADiskDescriptionVolumeUUIDKey);
        (!uuid.is_null() && CFGetTypeID(uuid) == CFUUIDGetTypeID())
            .then(|| CFUUIDGetUUIDBytes(uuid).0)
    });
    for object in [description, disk, Some(session)].into_iter().flatten() {
        unsafe { CFRelease(object) };
    }
    uuid.filter(|uuid| *uuid != [0; 16]).map(VolumeId::Uuid)
}

#[cfg(target_os = "linux")]
fn plain_volume_id(dir: BorrowedFd<'_>) -> Option<VolumeId> {
    btrfs_subvolume_id(dir)
        .or_else(|| ext_file_system_id(dir))
        .or_else(|| file_system_id(dir))
        .or_else(|| fat_volume_id(dir))
        .or_else(|| udev_volume_id(dir))
}

#[cfg(target_vendor = "apple")]
fn fat_kind(volume: &rustix::fs::StatFs) -> Option<u8> {
    let name: Vec<u8> = (volume.f_fstypename.iter())
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect();
    match name.as_slice() {
        b"msdos" => Some(1),
        b"exfat" => Some(2),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
fn fat_kind(volume: &rustix::fs::StatFs) -> Option<u8> {
    const MSDOS_SUPER_MAGIC: u64 = 0x4d44;
    const EXFAT_SUPER_MAGIC: u64 = 0x2011_bab0;
    match volume.f_type as u64 {
        MSDOS_SUPER_MAGIC => Some(1),
        EXFAT_SUPER_MAGIC => Some(2),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
fn volume_id_of(id: &[u8]) -> Option<VolumeId> {
    if id.len() > 16 || id.iter().all(|&byte| byte == 0) {
        return None;
    }
    let mut uuid = [0; 16];
    uuid[..id.len()].copy_from_slice(id);
    if id.len() < 16 {
        uuid[15] = id.len() as u8;
    }
    Some(VolumeId::Uuid(uuid))
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
    if done != 0 {
        return None;
    }
    volume_id_of(reply.uuid.get(..usize::from(reply.len))?)
}

#[cfg(target_os = "linux")]
fn fat_volume_id(dir: BorrowedFd<'_>) -> Option<VolumeId> {
    use std::os::fd::AsRawFd;
    const MSDOS_SUPER_MAGIC: u64 = 0x4d44;
    const FAT_IOCTL_GET_VOLUME_ID: libc::c_ulong = 0x8004_7213;
    if rustix::fs::fstatfs(dir).ok()?.f_type as u64 != MSDOS_SUPER_MAGIC {
        return None;
    }
    let mut serial: u32 = 0;
    let done = unsafe {
        libc::ioctl(
            dir.as_raw_fd(),
            FAT_IOCTL_GET_VOLUME_ID as _,
            &raw mut serial,
        )
    };
    if done != 0 {
        return None;
    }
    volume_id_of(&serial.to_le_bytes())
}

/// Whether the volume whose root folder `name` in `dir` names is FAT or
/// exFAT. Neither keeps a directory entry for its root folder, so the root has
/// no birth time, and Linux reports the time an exFAT volume was mounted
/// (<https://github.com/torvalds/linux/blob/v6.8/fs/exfat/super.c#L377>).
#[cfg(target_os = "linux")]
pub(crate) fn has_no_root_birth(dir: BorrowedFd<'_>, name: &OsStr, flags: AtFlags) -> bool {
    let volume = match name.is_empty() {
        true => rustix::fs::fstatfs(dir),
        false => {
            let nofollow = match flags.contains(AtFlags::SYMLINK_NOFOLLOW) {
                true => OFlags::NOFOLLOW,
                false => OFlags::empty(),
            };
            let opened = OFlags::PATH | OFlags::CLOEXEC | nofollow;
            rustix::fs::openat(dir, name, opened, Mode::empty())
                .and_then(|root| rustix::fs::fstatfs(&root))
        }
    };
    volume.is_ok_and(|stat| fat_kind(&stat).is_some())
}

/// udev keeps one `/dev/disk/by-uuid` link for every disk with that UUID, so
/// the UUID comes from udev's record of the device number, which lists its
/// properties and links (<https://github.com/systemd/systemd/blob/v255/src/libsystemd/sd-device/sd-device.c>).
/// udev keeps a record until it handles the event that removes its device, so
/// a disk that reuses a device number can find the record of the disk before
/// it. Linux raises a disk's `diskseq` for each new disk and media change, and
/// udev records the `/dev/disk/by-diskseq` link only once it has handled the
/// disk (<https://www.kernel.org/doc/Documentation/ABI/stable/sysfs-block>).
#[cfg(target_os = "linux")]
fn udev_volume_id(dir: BorrowedFd<'_>) -> Option<VolumeId> {
    let dev = stat_fd(dir).ok()?.dev;
    let device = format!("{}:{}", rustix::fs::major(dev), rustix::fs::minor(dev));
    let sys = Path::new("/sys/dev/block").join(&device);
    let read = |path: &Path| Some(std::fs::read_to_string(path).ok()?.trim().to_owned());
    let partition = read(&sys.join("partition"));
    let disk = if partition.is_some() {
        sys.join("..")
    } else {
        sys
    };
    let mut seq = read(&disk.join("diskseq"))?;
    if let Some(number) = partition {
        seq = format!("{seq}-part{number}");
    }
    let record = std::fs::read_to_string(format!("/run/udev/data/b{device}")).ok()?;
    let handled = format!("S:disk/by-diskseq/{seq}");
    if !record.lines().any(|line| line == handled) {
        return None;
    }
    record
        .lines()
        .find_map(|line| line.strip_prefix("E:ID_FS_UUID="))
        .and_then(udev_id)
}

/// libblkid prints a serial shorter than a UUID as the number, so its bytes are the reverse of the little-endian ones on disk.
#[cfg(target_os = "linux")]
fn udev_id(name: &str) -> Option<VolumeId> {
    let digits: String = name.chars().filter(|&c| c != '-').collect();
    if !digits.len().is_multiple_of(2) || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut id: Vec<u8> = (0..digits.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&digits[at..at + 2], 16))
        .collect::<Result<_, _>>()
        .ok()?;
    if id.len() < 16 {
        id.reverse();
    }
    volume_id_of(&id)
}

#[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
fn plain_volume_id(_: BorrowedFd<'_>) -> Option<VolumeId> {
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
    if linked.volume.is_none() || linked.volume != volume_ids(dir).map(|(_, plain)| plain) {
        linked.admits(&found)?;
    }
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

    /// The folder's token log, empty when there is none.
    pub fn lineage(&self) -> io::Result<Lineage> {
        match open_dir_at(self.root(), OsStr::new(PRIVATE_DIR)) {
            Ok(private) => Lineage::read(private.as_fd()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Lineage::default()),
            Err(err) => Err(err),
        }
    }

    /// Writes `next` to the token log as `rotation` says, then writes `marker` with `next` as its token. Fails,
    /// writing nothing, unless the root is held. A token log that can't be written only shortens how far back a
    /// restored store finds its own token, so the marker is written either way.
    pub fn write_rotation(
        &self,
        marker: &Marker,
        next: Minted,
        rotation: Rotation,
    ) -> io::Result<()> {
        self.check_held()?;
        let _ = write_lineage(private_dir(self.root())?.as_fd(), marker, next, rotation);
        let rotated = Marker {
            token: Some(next.to_string()),
            ..marker.clone()
        };
        write_marker_at(self.root(), &rotated, Flush::Full)
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

#[cfg(all(test, any(target_vendor = "apple", target_os = "linux")))]
#[test]
fn a_fat_volume_is_named_by_its_id_file_system_and_size() {
    let mut serial = [0; 16];
    serial[..4].copy_from_slice(&0x4b4f_4944_u32.to_le_bytes());
    serial[15] = 4;
    let plain = VolumeId::Uuid(serial);
    let sized = sized_volume_id(plain, 1, 16 << 20);
    assert_eq!(
        sized,
        VolumeId::Uuid([
            0xca, 0xfc, 0x64, 0xbb, 0x32, 0x76, 0x54, 0x1d, 0xbd, 0x82, 0x53, 0x2b, 0x92, 0xcd,
            0xe3, 0x72
        ])
    );
    assert_ne!(sized_volume_id(plain, 2, 16 << 20), sized);
    assert_ne!(sized_volume_id(plain, 1, 32 << 20), sized);
    assert_ne!(sized, plain);
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn udev_names_a_volume_by_the_id_the_kernel_reports() {
        assert_eq!(
            udev_id("ABCD-1234"),
            volume_id_of(&0xABCD_1234_u32.to_le_bytes())
        );
        assert_eq!(
            udev_id("0123456789ABCDEF"),
            volume_id_of(&0x0123_4567_89AB_CDEF_u64.to_le_bytes())
        );
        assert_eq!(
            udev_id("4d5a2860-ff6d-4f32-bf61-5f40c3a1e33a"),
            Some(VolumeId::Uuid([
                0x4d, 0x5a, 0x28, 0x60, 0xff, 0x6d, 0x4f, 0x32, 0xbf, 0x61, 0x5f, 0x40, 0xc3, 0xa1,
                0xe3, 0x3a
            ]))
        );
        for name in [
            "",
            "0000-0000",
            "ABC",
            "+1+2-3456",
            "C12A-244G",
            "0123456789abcdef0123456789abcdef01",
        ] {
            assert_eq!(udev_id(name), None, "{name}");
        }
    }

    #[test]
    fn short_volume_ids_of_other_lengths_differ() {
        let fat = volume_id_of(&[1, 2, 3, 4]).unwrap();
        let ntfs = volume_id_of(&[1, 2, 3, 4, 0, 0, 0, 0]).unwrap();
        assert_ne!(fat, ntfs);
        assert_eq!(VolumeId::from_bytes(fat.as_bytes()), Some(fat));
        assert_eq!(volume_id_of(&[0; 4]), None);
    }

    struct Attached(Vec<String>, std::path::PathBuf);

    impl Drop for Attached {
        fn drop(&mut self) {
            let sudo = || std::process::Command::new("sudo");
            let _ = sudo().args(["-n", "umount", "-l"]).arg(&self.1).status();
            for device in &self.0 {
                let _ = sudo().args(["-n", "losetup", "-d", device]).status();
            }
        }
    }

    #[test]
    fn a_disk_swapped_while_a_scan_reads_it_never_passes_for_the_disk_it_identified() {
        use std::ffi::OsString;
        use std::os::unix::fs::MetadataExt;
        use std::path::PathBuf;
        use std::process::Command;

        use crate::{Cut, Snapshot};
        let Some(disks) = koidrive_test_support::Disks::new() else {
            return;
        };
        let sudo = |args: &[&OsStr]| {
            let out = Command::new("sudo").arg("-n").args(args).output().unwrap();
            match out.status.success() {
                true => Ok(String::from_utf8(out.stdout).unwrap().trim().to_owned()),
                false => Err(format!(
                    "sudo {args:?}: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )),
            }
        };
        let must = |args: &[&OsStr]| sudo(args).unwrap_or_else(|failure| panic!("{failure}"));
        let owner = std::fs::metadata(disks.base()).unwrap();
        let options = format!("uid={},gid={}", owner.uid(), owner.gid());
        for stage in ["root opened", "disk opened", "removal confirmed"] {
            let (first, second) = (
                disks.fat(&format!("{stage} 1")),
                disks.fat(&format!("{stage} 2")),
            );
            let root = disks.base().join(stage);
            let mount = root.join("Disk");
            std::fs::create_dir_all(&mount).unwrap();
            let attach = |image: &Path| {
                let losetup = ["losetup", "--find", "--show"].map(OsStr::new);
                must(&[&losetup[..], &[image.as_os_str()]].concat())
            };
            let mut attached = Attached(vec![attach(&first)], mount.clone());
            let mount_on = |device: &str| {
                let args = ["mount", "-o", &options, device].map(OsStr::new);
                must(&[&args[..], &[mount.as_os_str()]].concat());
            };
            mount_on(&attached.0[0]);
            std::fs::write(mount.join("report.docx"), b"first disk").unwrap();
            std::fs::write(mount.join("old.txt"), b"old").unwrap();
            let mut snapshot = Snapshot::scan(&root).unwrap();
            std::fs::remove_file(mount.join("old.txt")).unwrap();
            let mut swapped = false;
            let result = snapshot.rescan_with(&root, [PathBuf::new()], None, &mut |cut| {
                let now = match cut {
                    Cut::RootOpened => stage == "root opened",
                    Cut::OpeningDir(path) => stage == "disk opened" && path == Path::new("Disk"),
                    Cut::Confirming(path) => {
                        stage == "removal confirmed" && path == Path::new("Disk")
                    }
                    Cut::Searching(_) => false,
                };
                if !now || swapped {
                    return;
                }
                let device = attached.0[0].clone();
                let replacement = match sudo(&["umount".as_ref(), mount.as_os_str()]) {
                    Ok(_) => {
                        let mut from = OsString::from("if=");
                        from.push(&second);
                        let to = format!("of={device}");
                        must(&[
                            "dd".as_ref(),
                            from.as_os_str(),
                            to.as_ref(),
                            "bs=1M".as_ref(),
                            "status=none".as_ref(),
                        ]);
                        device
                    }
                    Err(_) => {
                        must(&["umount".as_ref(), "-l".as_ref(), mount.as_os_str()]);
                        attached.0.push(attach(&second));
                        attached.0[1].clone()
                    }
                };
                mount_on(&replacement);
                swapped = !mount.join("report.docx").exists();
            });
            drop(attached);
            assert!(swapped, "{stage}");
            assert!(
                result.is_err() || snapshot.entries.contains_key(Path::new("Disk/report.docx")),
                "{stage}: {result:?}"
            );
        }
    }
}
