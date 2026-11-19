// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeSet, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use koidrive_ignore::PLATFORM_FILES;
use rustix::fs::{Access, AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;
use serde::de::IgnoredAny;

use crate::mark;
use crate::resolve::{Base, Folder, Location, PARKED_DIR, Space, Unresolved, check_name};
use crate::root::{
    Flush, Marker, PRIVATE_DIR, flush, remove_if_present, rename_no_replace, replace_file,
    write_marker_at,
};
use crate::trash::TRASH_DIR;
use crate::{Ctime, Entry, EntryKind, FileId, Stat, list, stat_at, stat_fd};

/// The folder inside the private folder that holds staged downloads.
pub const STAGING_DIR: &str = "staging";
/// The folder inside the private folder that holds displaced entries until
/// they are compared. It is never swept.
pub const QUARANTINE_DIR: &str = "quarantine";
type Held = [(Folder, BTreeSet<OsString>); 2];

const BROUGHT_BACK: &str = "brought-back.json";
const BROUGHT_BACK_TEMP: &str = "brought-back.json.tmp";

/// What [`Space::brought_back`] read of the entries a store rollback brought back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BroughtBack {
    Entries(HashSet<FileId>),
    /// The record exists but cannot be read, so no entry is known not to be one of them.
    Unreadable,
}

impl BroughtBack {
    /// Whether `id` may be an entry a store rollback brought back.
    pub fn holds(&self, id: FileId) -> bool {
        match self {
            BroughtBack::Entries(ids) => ids.contains(&id.without_handle()),
            BroughtBack::Unreadable => true,
        }
    }
}

/// An entry an effect displaces, as the caller last observed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Displaced {
    pub at: Location,
    /// Identity, kind, size, modification time and executable bit. The
    /// content is compared through [`Contents::displaced`].
    pub entry: Entry,
}

/// One disk change, in the terms of core/fs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    CreateDir {
        at: Location,
    },
    /// Puts the staged file `.koidrive/staging/<effect id>` at `at`. With
    /// `replaces`, that entry goes to quarantine first; it sits elsewhere when
    /// the node also moves. `staged` is the staged file's identity once the
    /// record holds it (design §5.3).
    WriteFile {
        at: Location,
        replaces: Option<Displaced>,
        staged: Option<FileId>,
    },
    /// Renames the entry with identity `id`.
    Move {
        from: Location,
        id: FileId,
        to: Location,
    },
    RemoveFile {
        displaced: Displaced,
    },
    /// Removes a folder, which must hold nothing but the files `along` once quarantined. They go to the trash inside
    /// it.
    RemoveDir {
        at: Location,
        id: FileId,
        along: Vec<Displaced>,
    },
}

/// A disk effect as its record holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effect {
    /// Names the effect's staging and quarantine entries, and its trash entry,
    /// so it must be a single file name.
    pub id: String,
    /// The node the effect is for. A file it places and a folder it creates are marked with it.
    pub node: String,
    pub action: Action,
}

/// Compares file contents with the ones an effect expects.
pub trait Contents {
    /// Whether `file` holds the content the effect displaces.
    fn displaced(&mut self, file: &File) -> io::Result<bool>;
    /// Whether `file` holds the content the effect writes.
    fn written(&mut self, file: &File) -> io::Result<bool>;
    /// Whether `file` holds the content of the file at `index` among the files a folder removal takes along.
    fn along(&mut self, index: usize, file: &File) -> io::Result<bool>;
    /// What the local trash records as the displaced file's content, for
    /// [`Space::reclaim_trash`] to compare the trashed file with later.
    fn displaced_content(&self) -> Option<Vec<u8>>;
}

/// The points between the steps of design §5.2 where a hook runs. Each is
/// reached once the step it names is complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The root marker holds the section's counter.
    Marked,
    /// The folders resolved and the entry the effect acts on matched.
    Checked,
    /// The displaced entry is in quarantine.
    Quarantined,
    /// The quarantined entry was compared with the expected one.
    Compared,
    /// The new entry has its name.
    Placed,
    /// The quarantined entry went back to its name or to a recovered name.
    Returned,
    /// The quarantined entry is in the local trash.
    Trashed,
}

/// How an effect ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// `identity` is the identity of the entry the effect created or wrote
    /// at its target, read through the descriptor that made or verified it:
    /// the staged file before its rename, the new folder through its parent,
    /// or the file whose content recovery compared. It is `None` for a move
    /// or a removal.
    Done { identity: Option<FileId> },
    /// The disk did not match what the effect expected. The caller observes
    /// the disk again; `recovered` is the path of a file made from the
    /// displaced entry, which is a new local file.
    Failed {
        reason: Reason,
        recovered: Option<PathBuf>,
    },
}

/// Why an effect failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// A folder on the way is missing, is not a folder, or is another one.
    Moved,
    /// The entry the effect acts on is not there.
    Missing,
    /// The entry the effect acts on differs from the one expected.
    Changed,
    /// The name the effect puts an entry at is taken.
    Taken,
    /// Recovery found the effect stopped before it displaced anything.
    Interrupted,
    /// The effect sent the entry it replaced to the local trash, and its
    /// target does not hold the new content: the user changed or removed the
    /// written file, or took its name before it was placed.
    Overtaken,
    /// The folder a removal acts on holds entries other than platform files and
    /// the files it takes along, named here in order with their kinds: none
    /// for an entry that is neither a file nor a folder, such as a symlink.
    /// The folder went back as it was.
    Holds(Vec<(OsString, Option<EntryKind>)>),
    /// The system did not allow the effect, as for a folder without write
    /// permission, a locked file or a read-only volume. Nothing changed.
    Denied,
}

/// Where the system would refuse a change that an action makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// A folder the action changes cannot be written, or an entry it moves,
    /// replaces or removes is locked.
    Here,
    /// Koidrive's own folder at this path, relative to the root, cannot be
    /// written.
    Private(PathBuf),
}

/// Effects whose steps become durable together (design §5.2), begun by
/// [`Space::section`]. Each step is ordered on disk against the steps it must
/// precede, and [`Section::finish`] makes all of them durable before it hands
/// out the outcomes, so no outcome can be acknowledged before then.
pub struct Section<'a> {
    space: &'a Space,
    hook: &'a mut dyn FnMut(Step),
    outcomes: Vec<Outcome>,
}

impl Section<'_> {
    /// Carries out `effect`.
    ///
    /// An error leaves the effect anywhere between its steps, and drops the
    /// outcomes of the section's earlier effects: recover each effect of the
    /// section in a new one before anything else. A replacement whose staged
    /// file is missing fails with [`io::ErrorKind::NotFound`] before it changes
    /// anything. A root that is no longer held fails the effect with
    /// [`Unresolved::RootWithdrawn`] before the next change.
    pub fn apply(
        &mut self,
        effect: &Effect,
        contents: &mut dyn Contents,
        now: SystemTime,
    ) -> io::Result<()> {
        let outcome = self.space.attempt(effect, contents, now, self.hook)?;
        self.outcomes.push(outcome);
        Ok(())
    }

    /// Recovers `effect` after a crash (design §5.3), from what the disk holds
    /// at its target, its staging path and its quarantine path. The section's
    /// marker covers it, since the crash may have come before the marker
    /// write. Recovery is idempotent: a crash during it is recovered by running
    /// it again. A root that is no longer held fails recovery with
    /// [`Unresolved::RootWithdrawn`] before the next change, and recovery runs
    /// again once the root is bound anew.
    pub fn recover(
        &mut self,
        effect: &Effect,
        contents: &mut dyn Contents,
        now: SystemTime,
    ) -> io::Result<()> {
        let outcome = self
            .space
            .recover_effect(effect, contents, now, self.hook)?;
        self.outcomes.push(outcome);
        Ok(())
    }

    /// Makes every step of the section durable and returns the outcomes of its
    /// effects, in the order they were applied or recovered.
    pub fn finish(self) -> io::Result<Vec<Outcome>> {
        flush(self.space.root(), Flush::Full)?;
        Ok(self.outcomes)
    }
}

fn failed(reason: Reason) -> Outcome {
    Outcome::Failed {
        reason,
        recovered: None,
    }
}

fn overtaken(placed: Outcome) -> Outcome {
    match placed {
        Outcome::Failed { .. } => failed(Reason::Overtaken),
        done => done,
    }
}

fn is_denied(outcome: &Outcome) -> bool {
    matches!(
        outcome,
        Outcome::Failed {
            reason: Reason::Denied,
            ..
        }
    )
}

#[derive(Clone, Copy, PartialEq)]
enum Change {
    AddsFile,
    AddsFolder,
    Removes,
}

#[cfg(target_os = "macos")]
fn access(change: Change) -> Access {
    let bit = |shift: u32| Access::from_bits_retain(1 << shift);
    let (write_ok, execute_ok, append_ok) = (bit(10), bit(11), bit(13));
    match change {
        Change::AddsFile => write_ok | execute_ok,
        Change::AddsFolder => append_ok | execute_ok,
        Change::Removes => execute_ok,
    }
}

#[cfg(not(target_os = "macos"))]
fn access(_: Change) -> Access {
    Access::WRITE_OK | Access::EXEC_OK
}

fn allows(folder: BorrowedFd, change: Change) -> io::Result<bool> {
    match rustix::fs::accessat(folder, c".", access(change), AtFlags::EACCESS) {
        Ok(()) | Err(Errno::NOENT) => Ok(true),
        Err(Errno::ACCESS | Errno::PERM | Errno::ROFS) => Ok(false),
        Err(err) => Err(err.into()),
    }
}

fn refused_at(unresolved: Unresolved) -> Option<PathBuf> {
    match unresolved {
        Unresolved::Io { path, err } if refused(&err) => Some(path),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
fn locked(folder: BorrowedFd, name: &OsStr) -> bool {
    let delete = Access::from_bits_retain(1 << 12);
    matches!(
        rustix::fs::accessat(folder, name, delete, AtFlags::EACCESS),
        Err(Errno::ACCESS | Errno::PERM)
    )
}

#[cfg(not(target_os = "macos"))]
fn locked(folder: BorrowedFd, name: &OsStr) -> bool {
    rustix::fs::accessat(folder, name, Access::WRITE_OK, AtFlags::EACCESS) == Err(Errno::PERM)
}

fn refused(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem
    )
}

fn denied(err: io::Error) -> io::Result<Outcome> {
    match refused(&err) {
        true => Ok(failed(Reason::Denied)),
        false => Err(err),
    }
}

fn unresolved(err: Unresolved) -> io::Result<Outcome> {
    match err {
        Unresolved::Missing(_) | Unresolved::NotAFolder(_) | Unresolved::Replaced { .. } => {
            Ok(failed(Reason::Moved))
        }
        err => Err(err.into()),
    }
}

macro_rules! open {
    ($opened:expr) => {
        match $opened {
            Ok(opened) => opened,
            Err(Unresolved::Io { err, .. }) if refused(&err) => return Ok(failed(Reason::Denied)),
            Err(err) => return Err(err.into()),
        }
    };
}

macro_rules! resolve {
    ($resolved:expr) => {
        match $resolved {
            Ok(resolved) => resolved,
            Err(Unresolved::Io { err, .. }) if refused(&err) => return Ok(failed(Reason::Denied)),
            Err(err) => return unresolved(err),
        }
    };
}

impl Space {
    /// Whether the system would refuse a change `action` makes, checked
    /// without changing anything. A place that cannot be found is no refusal:
    /// applying the action reports it.
    pub fn refuses(&self, action: &Action) -> io::Result<Option<Refusal>> {
        let private: &[(&str, Change)] = match action {
            Action::WriteFile { replaces: None, .. } => &[(STAGING_DIR, Change::AddsFile)],
            Action::WriteFile {
                replaces: Some(_), ..
            } => &[
                (STAGING_DIR, Change::AddsFile),
                (QUARANTINE_DIR, Change::AddsFile),
            ],
            Action::RemoveFile { .. } => &[(QUARANTINE_DIR, Change::AddsFile)],
            Action::RemoveDir { .. } => &[(QUARANTINE_DIR, Change::AddsFolder)],
            Action::CreateDir { .. } | Action::Move { .. } => &[],
        };
        for (dir, change) in private {
            let refused = match self.private_present(&[dir]) {
                Ok(folder) => {
                    (!allows(folder.fd(), *change)?).then(|| Path::new(PRIVATE_DIR).join(dir))
                }
                Err(Unresolved::Missing(_)) => match self.private_present(&[]) {
                    Ok(folder) => (!allows(folder.fd(), Change::AddsFolder)?)
                        .then(|| PathBuf::from(PRIVATE_DIR)),
                    Err(unresolved) => refused_at(unresolved),
                },
                Err(unresolved) => refused_at(unresolved),
            };
            if let Some(path) = refused {
                return Ok(Some(Refusal::Private(path)));
            }
        }
        let changed: Vec<(&Location, Change)> = match action {
            Action::CreateDir { at } => vec![(at, Change::AddsFolder)],
            Action::WriteFile { at, replaces, .. } => std::iter::once((at, Change::AddsFile))
                .chain(
                    replaces
                        .iter()
                        .map(|displaced| (&displaced.at, Change::Removes)),
                )
                .collect(),
            Action::Move { from, to, .. } => {
                let folder = (self.parent_present(from).ok())
                    .and_then(|(source, name)| lookup(source.fd(), name).ok().flatten())
                    .is_some_and(|stat| stat.file_type == FileType::Directory);
                let adds = match folder {
                    true => Change::AddsFolder,
                    false => Change::AddsFile,
                };
                vec![(to, adds), (from, Change::Removes)]
            }
            Action::RemoveFile { displaced } => vec![(&displaced.at, Change::Removes)],
            Action::RemoveDir { at, .. } => vec![(at, Change::Removes)],
        };
        for (at, change) in changed {
            let (parent, name) = match self.parent_present(at) {
                Ok(found) => found,
                Err(unresolved) => match refused_at(unresolved) {
                    Some(_) => return Ok(Some(Refusal::Here)),
                    None => continue,
                },
            };
            if !allows(parent.fd(), change)?
                || (change == Change::Removes && locked(parent.fd(), name))
            {
                return Ok(Some(Refusal::Here));
            }
        }
        Ok(None)
    }

    /// Creates the staged file for effect `id`, replacing a leftover one.
    pub fn stage(&self, id: &str) -> io::Result<File> {
        check_name(OsStr::new(id))?;
        let staging = self.private(&[STAGING_DIR])?;
        remove_if_present(staging.fd(), id)?;
        let flags =
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        Ok(rustix::fs::openat(staging.fd(), id, flags, Mode::from_raw_mode(0o644))?.into())
    }

    /// Flushes the staging folder, so that every staged file whose identity the store records next survives a power
    /// loss.
    pub fn sync_staging(&self) -> io::Result<()> {
        let staging = self.private(&[STAGING_DIR])?;
        self.flush_folder(&staging, Flush::Barrier)
    }

    /// Flushes `folder` after a change in it. A folder on another device than the root, such as a volume mounted
    /// below it, is flushed in full, since the full flush that ends a section reaches only the root's device.
    pub(crate) fn flush_folder(&self, folder: &Folder, how: Flush) -> io::Result<()> {
        match stat_fd(folder.fd())?.dev == self.dev() {
            true => flush(folder.fd(), how),
            false => flush(folder.fd(), Flush::Full),
        }
    }

    /// Opens the staged file of effect `id` that [`Space::stage`] created, to write more of its content.
    pub fn open_staged(&self, id: &str) -> io::Result<File> {
        check_name(OsStr::new(id))?;
        let staging = self.private(&[STAGING_DIR])?;
        koidrive_state_file::open_regular(staging.fd(), id, OFlags::WRONLY | OFlags::NOFOLLOW)
    }

    /// Sets the staged file's executable bit and flushes it, which design
    /// §5.2 requires before the file is renamed into place, and returns its
    /// identity for the effect's record. The caller has written and verified
    /// its content, and finishes the file before it begins the [`Section`]
    /// that places it.
    pub fn finish_staged(&self, id: &str, executable: bool) -> io::Result<FileId> {
        check_name(OsStr::new(id))?;
        let staging = self.private(&[STAGING_DIR])?;
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW;
        let file = koidrive_state_file::open_regular(staging.fd(), id, flags)?;
        let mode = rustix::fs::fstat(&file)?.st_mode & 0o7777;
        let mode = if executable {
            mode | (mode & 0o444) >> 2
        } else {
            mode & !0o111
        };
        rustix::fs::fchmod(&file, Mode::from_raw_mode(mode as _))?;
        flush(file.as_fd(), Flush::Plain)?;
        Ok(stat_fd(file.as_fd())?.id)
    }

    /// Deletes every staged file except `keep`, the effect in flight. Staged
    /// files only hold bytes downloaded from the server (design §5.3).
    pub fn clear_staging(&self, keep: Option<&str>) -> io::Result<Vec<OsString>> {
        let staging = self.private(&[STAGING_DIR])?;
        let mut cleared = Vec::new();
        for name in list(staging.fd())? {
            if keep.is_some_and(|keep| name == OsStr::new(keep)) {
                continue;
            }
            self.recheck(&staging)?;
            rustix::fs::unlinkat(staging.fd(), &name, AtFlags::empty())?;
            cleared.push(name);
        }
        Ok(cleared)
    }

    /// Moves every entry of the parked directory and of quarantine to the root under a recovered
    /// name made from `name` of its private name and identity, and returns the paths they took. A
    /// store older than the disk does not know what the disk parked, or what the last effect the
    /// root marker counts left in quarantine when it stopped between its steps, and scans read
    /// neither directory (design §6.4). Before it moves any, it adds their identities to the set
    /// [`Space::brought_back`] reads, since the place each takes is not one the user gave it.
    pub fn bring_back_all(
        &self,
        name: impl Fn(&OsStr, FileId) -> OsString,
        now: SystemTime,
    ) -> io::Result<Vec<PathBuf>> {
        let Some((root, held)) = self.set_aside()? else {
            return Ok(Vec::new());
        };
        let mut moved = Vec::new();
        for (private, entries) in &held {
            for entry in entries {
                moved.push(self.bring_back_entry(&root, private, entry, &name, now)?);
            }
        }
        Ok(moved)
    }

    /// Moves every entry back as [`Space::bring_back_all`] does, but goes on past an entry it cannot move, and hands
    /// `moved` the path each one took, or why it stayed, before it moves the next.
    pub fn bring_back_each(
        &self,
        name: impl Fn(&OsStr, FileId) -> OsString,
        now: SystemTime,
        mut moved: impl FnMut(io::Result<PathBuf>),
    ) -> io::Result<()> {
        let Some((root, held)) = self.set_aside()? else {
            return Ok(());
        };
        for (private, entries) in &held {
            for entry in entries {
                moved(self.bring_back_entry(&root, private, entry, &name, now));
            }
        }
        Ok(())
    }

    fn bring_back_entry(
        &self,
        root: &Folder,
        private: &Folder,
        entry: &OsStr,
        name: &impl Fn(&OsStr, FileId) -> OsString,
        now: SystemTime,
    ) -> io::Result<PathBuf> {
        let stat = stat_at(private.fd(), entry)?;
        let original = name(entry, stat.id);
        let folder = stat.file_type == FileType::Directory;
        self.move_to_recovered_name(private, entry, root, &original, folder, now)
    }

    fn set_aside(&self) -> io::Result<Option<(Folder, Held)>> {
        let root = self.root_folder()?;
        let held = [PARKED_DIR, QUARANTINE_DIR].map(|dir| self.private(&[dir]));
        let [parked, quarantine] = held.map(|private| -> io::Result<_> {
            let private = private?;
            let entries = list(private.fd())?;
            Ok((private, entries))
        });
        let held = [parked?, quarantine?];
        if held.iter().all(|(_, entries)| entries.is_empty()) {
            return Ok(None);
        }
        if let BroughtBack::Entries(mut brought_back) = self.brought_back()? {
            for (private, entries) in &held {
                for entry in entries {
                    brought_back.insert(stat_at(private.fd(), entry)?.id);
                }
            }
            let ids: Vec<_> = (brought_back.iter())
                .map(|id| (0, id.ino, id.birth_ns))
                .collect();
            replace_file(
                self.private(&[])?.fd(),
                BROUGHT_BACK,
                BROUGHT_BACK_TEMP,
                &serde_json::to_vec(&ids)?,
                Flush::Barrier,
            )?;
        }
        Ok(Some((root, held)))
    }

    /// The identities of the entries [`Space::bring_back_all`] moved. The record is never pruned,
    /// since no scan can show that an entry and everything in it lost the mark it had. Recovery does not find an entry by its node mark when the
    /// entry, or a folder it is in, is one of them (design §6.4). The set is kept on disk, so a
    /// stop between bringing entries back and recovering keeps it. A record that cannot be read
    /// is [`BroughtBack::Unreadable`], and a later bring-back leaves it so.
    pub fn brought_back(&self) -> io::Result<BroughtBack> {
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW;
        let file =
            match koidrive_state_file::open_regular(self.private(&[])?.fd(), BROUGHT_BACK, flags) {
                Ok(file) => file,
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    return Ok(BroughtBack::Entries(HashSet::new()));
                }
                Err(_) => return Ok(BroughtBack::Unreadable),
            };
        let Ok(ids) = serde_json::from_reader::<_, Vec<(IgnoredAny, u128, Option<i64>)>>(file)
        else {
            return Ok(BroughtBack::Unreadable);
        };
        Ok(BroughtBack::Entries(
            (ids.into_iter())
                .map(|(_, ino, birth_ns)| FileId {
                    ino,
                    birth_ns,
                    handle: None,
                })
                .collect(),
        ))
    }

    /// Replaces a record [`Space::brought_back`] cannot read with the identities of every entry
    /// of the root, so no entry it may have held is found by its node mark.
    pub fn repair_brought_back(&self) -> io::Result<()> {
        if self.brought_back()? != BroughtBack::Unreadable {
            return Ok(());
        }
        let root = self.root_folder()?;
        let mut ids = Vec::new();
        for entry in list(root.fd())? {
            if entry != PRIVATE_DIR {
                let id = stat_at(root.fd(), &entry)?.id;
                ids.push((0, id.ino, id.birth_ns));
            }
        }
        replace_file(
            self.private(&[])?.fd(),
            BROUGHT_BACK,
            BROUGHT_BACK_TEMP,
            &serde_json::to_vec(&ids)?,
            Flush::Barrier,
        )
    }

    /// Begins an effect section after checking that the root is held. It
    /// writes `marker`, which holds the counter the store sets once it
    /// acknowledges the section's effects (design §2.8), and orders the
    /// marker and every staged file finished before it ahead of the section's
    /// changes. `hook` runs at every [`Step`] the section reaches.
    pub fn section<'a>(
        &'a self,
        marker: &Marker,
        hook: &'a mut dyn FnMut(Step),
    ) -> io::Result<Section<'a>> {
        self.check_held()?;
        write_marker_at(self.root(), marker, Flush::Barrier)?;
        hook(Step::Marked);
        Ok(Section {
            space: self,
            hook,
            outcomes: Vec::new(),
        })
    }

    fn attempt(
        &self,
        effect: &Effect,
        contents: &mut dyn Contents,
        now: SystemTime,
        hook: &mut dyn FnMut(Step),
    ) -> io::Result<Outcome> {
        check_name(OsStr::new(&effect.id))?;
        self.check_held()?;
        let outcome = match &effect.action {
            Action::CreateDir { at } => {
                let (parent, name) = resolve!(self.parent(at));
                hook(Step::Checked);
                resolve!(self.recheck(&parent));
                match rustix::fs::mkdirat(parent.fd(), name, Mode::from_raw_mode(0o755)) {
                    Ok(()) => self.flush_folder(&parent, Flush::Plain)?,
                    Err(Errno::EXIST) => return Ok(failed(Reason::Taken)),
                    Err(err) => return denied(err.into()),
                }
                let folder = mark::open(parent.fd(), name)?.ok_or(io::ErrorKind::NotFound)?;
                let identity = stat_fd(folder.as_fd())?.id;
                mark::write(folder.as_fd(), &effect.node);
                hook(Step::Placed);
                Outcome::Done {
                    identity: Some(identity),
                }
            }
            Action::WriteFile {
                at, replaces: None, ..
            } => {
                let staging = open!(self.private(&[STAGING_DIR]));
                let (parent, name) = resolve!(self.parent(at));
                hook(Step::Checked);
                self.place(&staging, effect, &parent, name, hook)?
            }
            Action::WriteFile {
                at,
                replaces: Some(displaced),
                ..
            } => self.displace(effect, displaced, Some(at), contents, now, hook)?,
            Action::RemoveFile { displaced } => {
                self.displace(effect, displaced, None, contents, now, hook)?
            }
            Action::RemoveDir { at, id, .. } => {
                let displaced = Displaced {
                    at: at.clone(),
                    entry: folder_entry(*id),
                };
                self.displace(effect, &displaced, None, contents, now, hook)?
            }
            Action::Move { from, id, to } => {
                let (source, name) = resolve!(self.parent(from));
                let (target, new_name) = resolve!(self.parent(to));
                match lookup(source.fd(), name) {
                    Err(err) => return denied(err),
                    Ok(None) => return Ok(failed(Reason::Missing)),
                    Ok(Some(found)) if !found.id.same(id) => return Ok(failed(Reason::Changed)),
                    Ok(Some(_)) => {}
                }
                hook(Step::Checked);
                resolve!(self.recheck(&source));
                resolve!(self.recheck(&target));
                match rename_no_replace(source.fd(), name, target.fd(), new_name) {
                    Ok(()) => {}
                    Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                        return Ok(failed(Reason::Taken));
                    }
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {
                        return Ok(failed(Reason::Missing));
                    }
                    Err(err) => return denied(err),
                }
                self.flush_folder(&source, Flush::Plain)?;
                self.flush_folder(&target, Flush::Plain)?;
                hook(Step::Placed);
                Outcome::Done { identity: None }
            }
        };
        Ok(outcome)
    }

    fn displace(
        &self,
        effect: &Effect,
        displaced: &Displaced,
        place: Option<&Location>,
        contents: &mut dyn Contents,
        now: SystemTime,
        hook: &mut dyn FnMut(Step),
    ) -> io::Result<Outcome> {
        let (parent, name) = resolve!(self.parent(&displaced.at));
        let target = match place {
            Some(at) => {
                let target = resolve!(self.parent(at));
                let staging = open!(self.private(&[STAGING_DIR]));
                if lookup(staging.fd(), OsStr::new(&effect.id))?.is_none() {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("effect {} has no staged file", effect.id),
                    ));
                }
                Some((staging, target))
            }
            None => None,
        };
        let quarantine = open!(self.private(&[QUARANTINE_DIR]));
        match lookup(parent.fd(), name) {
            Err(err) => return denied(err),
            Ok(None) => return Ok(failed(Reason::Missing)),
            Ok(Some(found)) if found.entry() != Some(displaced.entry) => {
                return Ok(failed(Reason::Changed));
            }
            Ok(Some(_)) => {}
        }
        let along = match &effect.action {
            Action::RemoveDir { along, .. } => along.as_slice(),
            _ => &[],
        };
        if displaced.entry.kind == EntryKind::Dir
            && let Some(reason) = held(parent.fd(), name, along)?
        {
            return Ok(failed(reason));
        }
        hook(Step::Checked);

        resolve!(self.recheck(&parent));
        open!(self.recheck(&quarantine));
        let id = OsStr::new(&effect.id);
        match rename_no_replace(parent.fd(), name, quarantine.fd(), id) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok(failed(Reason::Missing));
            }
            Err(err) => return denied(err),
        }
        self.flush_folder(&parent, Flush::Plain)?;
        self.flush_folder(&quarantine, Flush::Barrier)?;
        hook(Step::Quarantined);

        let differs = match differs(quarantine.fd(), id, &displaced.entry, along, contents) {
            Ok(differs) => differs,
            Err(err) => {
                self.put_back(&quarantine, id, &displaced.at, now)?;
                return Err(err);
            }
        };
        hook(Step::Compared);
        if let Some(reason) = differs {
            let recovered = self.put_back(&quarantine, id, &displaced.at, now)?;
            hook(Step::Returned);
            return Ok(Outcome::Failed { reason, recovered });
        }
        let outcome = match target {
            Some((staging, (target, new_name))) => {
                match self.place(&staging, effect, &target, new_name, hook)? {
                    placed if is_denied(&placed) => {
                        return self.returned(
                            &quarantine,
                            effect,
                            displaced,
                            Reason::Denied,
                            now,
                            hook,
                        );
                    }
                    placed => overtaken(placed),
                }
            }
            None => Outcome::Done { identity: None },
        };
        if displaced.entry.kind == EntryKind::Dir {
            clear_platform_files(quarantine.fd(), id)?;
        }
        let content = contents.displaced_content();
        self.trash(&quarantine, id, &effect.id, name, content.as_deref(), now)?;
        hook(Step::Trashed);
        Ok(outcome)
    }

    fn place(
        &self,
        staging: &Folder,
        effect: &Effect,
        target: &Folder,
        name: &OsStr,
        hook: &mut dyn FnMut(Step),
    ) -> io::Result<Outcome> {
        resolve!(self.recheck(target));
        self.recheck(staging)?;
        let id = OsStr::new(&effect.id);
        let staged = mark::open(staging.fd(), id)?.ok_or(io::ErrorKind::NotFound)?;
        let identity = stat_fd(staged.as_fd())?.id;
        mark::write(staged.as_fd(), &effect.node);
        match rename_no_replace(staging.fd(), id, target.fd(), name) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                return Ok(failed(Reason::Taken));
            }
            Err(err) => return denied(err),
        }
        self.flush_folder(staging, Flush::Plain)?;
        self.flush_folder(target, Flush::Plain)?;
        hook(Step::Placed);
        Ok(Outcome::Done {
            identity: Some(identity),
        })
    }

    fn returned(
        &self,
        quarantine: &Folder,
        effect: &Effect,
        displaced: &Displaced,
        reason: Reason,
        now: SystemTime,
        hook: &mut dyn FnMut(Step),
    ) -> io::Result<Outcome> {
        let id = OsStr::new(&effect.id);
        let recovered = self.put_back(quarantine, id, &displaced.at, now)?;
        hook(Step::Returned);
        Ok(Outcome::Failed { reason, recovered })
    }

    fn put_back(
        &self,
        quarantine: &Folder,
        id: &OsStr,
        at: &Location,
        now: SystemTime,
    ) -> io::Result<Option<PathBuf>> {
        let folder = stat_at(quarantine.fd(), id)?.file_type == FileType::Directory;
        self.recheck(quarantine)?;
        let (parent, name) = match self.parent(at).and_then(|(parent, name)| {
            self.recheck(&parent)?;
            Ok((parent, name))
        }) {
            Ok((parent, name)) => (parent, name),
            Err(
                Unresolved::Missing(_) | Unresolved::NotAFolder(_) | Unresolved::Replaced { .. },
            ) => {
                let original = location_name(at);
                let root = self.root_folder()?;
                let path =
                    self.move_to_recovered_name(quarantine, id, &root, original, folder, now)?;
                return Ok(Some(path));
            }
            Err(err) => return Err(err.into()),
        };
        match rename_no_replace(quarantine.fd(), id, parent.fd(), name) {
            Ok(()) => {
                self.flush_folder(quarantine, Flush::Plain)?;
                self.flush_folder(&parent, Flush::Barrier)?;
                Ok(None)
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                let path =
                    self.move_to_recovered_name(quarantine, id, &parent, name, folder, now)?;
                Ok(Some(path))
            }
            Err(err) => Err(err),
        }
    }

    fn recover_effect(
        &self,
        effect: &Effect,
        contents: &mut dyn Contents,
        now: SystemTime,
        hook: &mut dyn FnMut(Step),
    ) -> io::Result<Outcome> {
        check_name(OsStr::new(&effect.id))?;
        self.check_held()?;
        let quarantine = self.private(&[QUARANTINE_DIR])?;
        let staging = self.private(&[STAGING_DIR])?;
        let id = OsStr::new(&effect.id);
        let staged = lookup(staging.fd(), id)?.is_some();
        let recorded = match &effect.action {
            Action::WriteFile { staged, .. } => *staged,
            _ => None,
        };
        let displacing = match &effect.action {
            Action::WriteFile {
                at,
                replaces: Some(displaced),
                ..
            } => Some((displaced.clone(), Some(at))),
            Action::RemoveFile { displaced } => Some((displaced.clone(), None)),
            Action::RemoveDir { at, id, .. } => Some((
                Displaced {
                    at: at.clone(),
                    entry: folder_entry(*id),
                },
                None,
            )),
            _ => None,
        };
        if let Some((displaced, place)) = displacing
            && lookup(quarantine.fd(), id)?.is_some()
        {
            let along = match &effect.action {
                Action::RemoveDir { along, .. } => along.as_slice(),
                _ => &[],
            };
            let differs = match differs(quarantine.fd(), id, &displaced.entry, along, contents) {
                Ok(differs) => differs,
                Err(err) => {
                    self.returned(&quarantine, effect, &displaced, Reason::Changed, now, hook)?;
                    return Err(err);
                }
            };
            hook(Step::Compared);
            if let Some(reason) = differs {
                return self.returned(&quarantine, effect, &displaced, reason, now, hook);
            }
            let outcome = match place {
                Some(at) if staged => {
                    let placed = match self.parent(at) {
                        Ok((target, name)) => self.place(&staging, effect, &target, name, hook)?,
                        Err(Unresolved::Io { err, .. }) if refused(&err) => failed(Reason::Denied),
                        Err(err) => unresolved(err)?,
                    };
                    if is_denied(&placed) {
                        return self.returned(
                            &quarantine,
                            effect,
                            &displaced,
                            Reason::Denied,
                            now,
                            hook,
                        );
                    }
                    overtaken(placed)
                }
                Some(_) if recorded.is_some() => Outcome::Done { identity: recorded },
                Some(at) => match self.written_at(at, contents)? {
                    Some(identity) => Outcome::Done {
                        identity: Some(identity),
                    },
                    None => failed(Reason::Overtaken),
                },
                None => Outcome::Done { identity: None },
            };
            let original = location_name(&displaced.at);
            if displaced.entry.kind == EntryKind::Dir {
                clear_platform_files(quarantine.fd(), id)?;
            }
            let content = contents.displaced_content();
            self.trash(
                &quarantine,
                id,
                &effect.id,
                original,
                content.as_deref(),
                now,
            )?;
            hook(Step::Trashed);
            return Ok(outcome);
        }
        if !staged && let Some(done) = self.finished(effect, contents)? {
            return Ok(done);
        }
        let replaced = matches!(
            effect.action,
            Action::WriteFile {
                replaces: Some(_),
                ..
            }
        );
        if staged && replaced && self.trashed(&effect.id)? {
            return Ok(failed(Reason::Overtaken));
        }
        Ok(failed(Reason::Interrupted))
    }

    fn finished(
        &self,
        effect: &Effect,
        contents: &mut dyn Contents,
    ) -> io::Result<Option<Outcome>> {
        let entry_at = |at: &Location| -> io::Result<Option<Option<Stat>>> {
            match self.found(at) {
                Ok((parent, name)) => Ok(Some(lookup(parent.fd(), name)?)),
                Err(
                    Unresolved::Missing(_)
                    | Unresolved::NotAFolder(_)
                    | Unresolved::Replaced { .. },
                ) => Ok(None),
                Err(err) => Err(err.into()),
            }
        };
        let trashed = || self.trashed(&effect.id);
        let identity = match &effect.action {
            Action::CreateDir { at } => match entry_at(at)? {
                Some(Some(stat)) if stat.file_type == FileType::Directory => Some(stat.id),
                _ => return Ok(None),
            },
            Action::WriteFile {
                staged: Some(identity),
                ..
            } => Some(*identity),
            Action::WriteFile {
                replaces: Some(_), ..
            } if !trashed()? => return Ok(None),
            Action::WriteFile { at, replaces, .. } => match self.written_at(at, contents)? {
                Some(identity) => Some(identity),
                None if replaces.is_some() => {
                    return Ok(Some(failed(Reason::Overtaken)));
                }
                None => return Ok(None),
            },
            Action::Move { to, id, .. } => match entry_at(to)? {
                Some(Some(stat)) if stat.id.same(id) => None,
                _ => return Ok(None),
            },
            Action::RemoveFile { .. } | Action::RemoveDir { .. } if trashed()? => None,
            Action::RemoveFile {
                displaced: Displaced { at, .. },
            }
            | Action::RemoveDir { at, .. } => match entry_at(at)? {
                Some(None) => return Ok(Some(failed(Reason::Missing))),
                _ => return Ok(None),
            },
        };
        Ok(Some(Outcome::Done { identity }))
    }

    fn trashed(&self, id: &str) -> io::Result<bool> {
        let trash = self.private(&[TRASH_DIR])?;
        Ok(lookup(trash.fd(), OsStr::new(id))?.is_some())
    }

    fn found<'a>(&self, at: &'a Location) -> Result<(Folder, &'a OsStr), Unresolved> {
        let resolved = self.parent(at);
        let moved = matches!(
            resolved,
            Err(Unresolved::Missing(_) | Unresolved::NotAFolder(_) | Unresolved::Replaced { .. })
        );
        let (
            true,
            Location::At {
                base: Base::Root,
                dirs,
                name,
            },
        ) = (moved, at)
        else {
            return resolved;
        };
        let Some(dir) = dirs.last() else {
            return resolved;
        };
        match self.find_folder(dir.id)? {
            Some(folder) => Ok((folder, name)),
            None => resolved,
        }
    }

    fn written_at(&self, at: &Location, contents: &mut dyn Contents) -> io::Result<Option<FileId>> {
        let (parent, name) = match self.found(at) {
            Ok(resolved) => resolved,
            Err(
                Unresolved::Missing(_) | Unresolved::NotAFolder(_) | Unresolved::Replaced { .. },
            ) => {
                return Ok(None);
            }
            Err(err) => return Err(err.into()),
        };
        match lookup(parent.fd(), name)? {
            Some(stat) if stat.file_type == FileType::RegularFile => {
                let file = open_file(parent.fd(), name)?;
                if !contents.written(&file)? {
                    return Ok(None);
                }
                Ok(Some(stat_fd(file.as_fd())?.id))
            }
            _ => Ok(None),
        }
    }

    /// Deletes the staged file of effect `id`.
    pub fn drop_staged(&self, id: &str) -> io::Result<()> {
        let dropped = (self.private(&[STAGING_DIR]).map_err(io::Error::from))
            .and_then(|staging| remove_if_present(staging.fd(), id));
        match dropped {
            Err(err) if refused(&err) => Ok(()),
            dropped => dropped,
        }
    }
}

fn differs(
    quarantine: BorrowedFd<'_>,
    id: &OsStr,
    expected: &Entry,
    along: &[Displaced],
    contents: &mut dyn Contents,
) -> io::Result<Option<Reason>> {
    match compare(quarantine, id, expected, along, contents) {
        Err(err) if refused(&err) => Ok(Some(Reason::Denied)),
        compared => compared,
    }
}

fn compare(
    quarantine: BorrowedFd<'_>,
    id: &OsStr,
    expected: &Entry,
    along: &[Displaced],
    contents: &mut dyn Contents,
) -> io::Result<Option<Reason>> {
    if lookup(quarantine, id)?.and_then(|stat| stat.entry()) != Some(*expected) {
        return Ok(Some(Reason::Changed));
    }
    match expected.kind {
        EntryKind::File => {
            let same = contents.displaced(&open_file(quarantine, id)?)?;
            Ok((!same).then_some(Reason::Changed))
        }
        EntryKind::Dir => {
            if let Some(reason) = held(quarantine, id, along)? {
                return Ok(Some(reason));
            }
            let dir = rustix::fs::openat(quarantine, id, crate::DIR_FLAGS, Mode::empty())?;
            for (index, file) in along.iter().enumerate() {
                let name = location_name(&file.at);
                if lookup(dir.as_fd(), name)?.is_some()
                    && !contents.along(index, &open_file(dir.as_fd(), name)?)?
                {
                    return Ok(Some(Reason::Changed));
                }
            }
            Ok(None)
        }
    }
}

impl Space {
    /// The entries of the folder `dirs` resolve to from `base`, each with its kind: none for an entry that is neither
    /// a file nor a folder.
    pub fn entries(
        &self,
        base: &Base,
        dirs: &[crate::Dir],
    ) -> Result<Vec<(OsString, Option<EntryKind>)>, Unresolved> {
        let folder = self.folder(base, dirs)?;
        let failed = |err| Unresolved::Io {
            path: folder.path(),
            err,
        };
        let mut entries = Vec::new();
        for name in list(folder.fd()).map_err(failed)? {
            let Some(stat) = lookup(folder.fd(), &name).map_err(failed)? else {
                continue;
            };
            let kind = match stat.file_type {
                FileType::Directory => Some(EntryKind::Dir),
                FileType::RegularFile => Some(EntryKind::File),
                _ => None,
            };
            entries.push((name, kind));
        }
        Ok(entries)
    }
}

fn held(parent: BorrowedFd<'_>, name: &OsStr, along: &[Displaced]) -> io::Result<Option<Reason>> {
    let dir = rustix::fs::openat(parent, name, crate::DIR_FLAGS, Mode::empty())?;
    let mut held = Vec::new();
    for name in list(dir.as_fd())? {
        if let Some(file) = along.iter().find(|file| location_name(&file.at) == name) {
            if lookup(dir.as_fd(), &name)?.and_then(|stat| stat.entry()) != Some(file.entry) {
                return Ok(Some(Reason::Changed));
            }
            continue;
        }
        let platform = PLATFORM_FILES.iter().any(|file| name == OsStr::new(file));
        let Some(stat) = lookup(dir.as_fd(), &name)? else {
            continue;
        };
        let file_type = stat.file_type;
        if !platform || file_type != FileType::RegularFile {
            let kind = match file_type {
                FileType::Directory => Some(EntryKind::Dir),
                FileType::RegularFile => Some(EntryKind::File),
                _ => None,
            };
            held.push((name, kind));
        }
    }
    Ok((!held.is_empty()).then_some(Reason::Holds(held)))
}

fn clear_platform_files(quarantine: BorrowedFd<'_>, id: &OsStr) -> io::Result<()> {
    let dir = rustix::fs::openat(quarantine, id, crate::DIR_FLAGS, Mode::empty())?;
    if list(dir.as_fd())?.is_empty() {
        return Ok(());
    }
    for name in PLATFORM_FILES {
        remove_if_present(dir.as_fd(), name)?;
    }
    flush(dir.as_fd(), Flush::Plain)
}

fn lookup(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<Option<Stat>> {
    match stat_at(dir, name) {
        Ok(stat) => Ok(Some(stat)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

fn open_file(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<File> {
    koidrive_state_file::open_regular(dir, name, OFlags::RDONLY | OFlags::NOFOLLOW)
}

fn folder_entry(id: FileId) -> Entry {
    Entry {
        kind: EntryKind::Dir,
        id,
        size: 0,
        mtime_ns: 0,
        ctime: Ctime::Settled { at: 0, mode: 0 },
        executable: false,
        linked: false,
        mount: None,
    }
}

fn location_name(at: &Location) -> &OsStr {
    match at {
        Location::Parked(name) | Location::At { name, .. } => name,
    }
}
