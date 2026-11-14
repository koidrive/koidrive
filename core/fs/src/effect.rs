// SPDX-License-Identifier: Apache-2.0

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::PathBuf;
use std::time::SystemTime;

use koidrive_ignore::PLATFORM_FILES;
use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;

use crate::resolve::{Folder, Location, PARKED_DIR, Space, Unresolved, check_name};
use crate::root::{Flush, Marker, flush, remove_if_present, rename_no_replace, write_marker_at};
use crate::trash::TRASH_DIR;
use crate::{Entry, EntryKind, FileId, Stat, list, stat_at, stat_fd};

/// The folder inside the private folder that holds staged downloads.
pub const STAGING_DIR: &str = "staging";
/// The folder inside the private folder that holds displaced entries until
/// they are compared. It is never swept.
pub const QUARANTINE_DIR: &str = "quarantine";

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
    /// the node also moves.
    WriteFile {
        at: Location,
        replaces: Option<Displaced>,
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
    /// Removes a folder, which must be empty once quarantined.
    RemoveDir {
        at: Location,
        id: FileId,
    },
}

/// A disk effect as its record holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effect {
    /// Names the effect's staging and quarantine entries, and its trash entry,
    /// so it must be a single file name.
    pub id: String,
    pub action: Action,
}

/// Compares file contents with the ones an effect expects.
pub trait Contents {
    /// Whether `file` holds the content the effect displaces.
    fn displaced(&mut self, file: &File) -> io::Result<bool>;
    /// Whether `file` holds the content the effect writes.
    fn written(&mut self, file: &File) -> io::Result<bool>;
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
    /// The folder a removal acts on holds entries other than platform files,
    /// named here in order. The folder went back as it was.
    Holds(Vec<OsString>),
    /// The system did not allow the effect's first change, as for a folder
    /// without write permission, a locked file or a read-only volume. Nothing
    /// changed.
    Denied,
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
        let outcome = self.space.apply_effect(effect, contents, now, self.hook)?;
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
    /// Creates the staged file for effect `id`, replacing a leftover one.
    pub fn stage(&self, id: &str) -> io::Result<File> {
        check_name(OsStr::new(id))?;
        let staging = self.private(&[STAGING_DIR])?;
        remove_if_present(staging.fd(), id)?;
        let flags =
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        Ok(rustix::fs::openat(staging.fd(), id, flags, Mode::from_raw_mode(0o644))?.into())
    }

    /// Flushes `folder` after a change in it. A folder on another device than the root, such as a volume mounted
    /// below it, is flushed in full, since the full flush that ends a section reaches only the root's device.
    pub(crate) fn flush_folder(&self, folder: &Folder, how: Flush) -> io::Result<()> {
        match folder.id().dev == self.id().dev {
            true => flush(folder.fd(), how),
            false => flush(folder.fd(), Flush::Full),
        }
    }

    /// Opens the staged file of effect `id` that [`Space::stage`] created, to write more of its content.
    pub fn open_staged(&self, id: &str) -> io::Result<File> {
        check_name(OsStr::new(id))?;
        let staging = self.private(&[STAGING_DIR])?;
        let flags = OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        Ok(rustix::fs::openat(staging.fd(), id, flags, Mode::empty())?.into())
    }

    /// Sets the staged file's executable bit and flushes it, which design
    /// §5.2 requires before the file is renamed into place. The caller has
    /// written and verified its content, and finishes the file before it
    /// begins the [`Section`] that places it.
    pub fn finish_staged(&self, id: &str, executable: bool) -> io::Result<()> {
        check_name(OsStr::new(id))?;
        let staging = self.private(&[STAGING_DIR])?;
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let file = rustix::fs::openat(staging.fd(), id, flags, Mode::empty())?;
        let mode = rustix::fs::fstat(&file)?.st_mode & 0o7777;
        let mode = if executable {
            mode | (mode & 0o444) >> 2
        } else {
            mode & !0o111
        };
        rustix::fs::fchmod(&file, Mode::from_raw_mode(mode as _))?;
        flush(rustix::fd::AsFd::as_fd(&file), Flush::Plain)
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
    /// neither directory (design §6.4).
    pub fn bring_back_all(
        &self,
        name: impl Fn(&OsStr, FileId) -> OsString,
        now: SystemTime,
    ) -> io::Result<Vec<PathBuf>> {
        let root = self.root_folder()?;
        let mut moved = Vec::new();
        for dir in [PARKED_DIR, QUARANTINE_DIR] {
            let private = self.private(&[dir])?;
            for entry in list(private.fd())? {
                let stat = stat_at(private.fd(), &entry)?;
                let original = name(&entry, stat.id);
                let folder = stat.file_type == FileType::Directory;
                moved.push(
                    self.move_to_recovered_name(&private, &entry, &root, &original, folder, now)?,
                );
            }
        }
        Ok(moved)
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

    fn apply_effect(
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
                let identity = stat_at(parent.fd(), name)?.id;
                hook(Step::Placed);
                Outcome::Done {
                    identity: Some(identity),
                }
            }
            Action::WriteFile { at, replaces: None } => {
                let staging = self.private(&[STAGING_DIR])?;
                let (parent, name) = resolve!(self.parent(at));
                hook(Step::Checked);
                self.place(&staging, &effect.id, &parent, name, hook)?
            }
            Action::WriteFile {
                at,
                replaces: Some(displaced),
            } => self.displace(effect, displaced, Some(at), contents, now, hook)?,
            Action::RemoveFile { displaced } => {
                self.displace(effect, displaced, None, contents, now, hook)?
            }
            Action::RemoveDir { at, id } => {
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
                    Ok(Some(found)) if found.id != *id => return Ok(failed(Reason::Changed)),
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
        if !matches!(outcome, Outcome::Done { .. }) {
            self.drop_staged(&effect.id)?;
        }
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
                let staging = self.private(&[STAGING_DIR])?;
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
        let quarantine = self.private(&[QUARANTINE_DIR])?;
        match lookup(parent.fd(), name) {
            Err(err) => return denied(err),
            Ok(None) => return Ok(failed(Reason::Missing)),
            Ok(Some(found)) if found.entry() != Some(displaced.entry) => {
                return Ok(failed(Reason::Changed));
            }
            Ok(Some(_)) => {}
        }
        hook(Step::Checked);

        resolve!(self.recheck(&parent));
        self.recheck(&quarantine)?;
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

        let differs = differs(quarantine.fd(), id, &displaced.entry, contents)?;
        hook(Step::Compared);
        if let Some(reason) = differs {
            let recovered = self.put_back(&quarantine, id, &displaced.at, now)?;
            hook(Step::Returned);
            return Ok(Outcome::Failed { reason, recovered });
        }
        let outcome = match target {
            Some((staging, (target, new_name))) => {
                match self.place(&staging, &effect.id, &target, new_name, hook)? {
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
        if !matches!(outcome, Outcome::Done { .. }) {
            self.drop_staged(&effect.id)?;
        }
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
        id: &str,
        target: &Folder,
        name: &OsStr,
        hook: &mut dyn FnMut(Step),
    ) -> io::Result<Outcome> {
        resolve!(self.recheck(target));
        self.recheck(staging)?;
        let identity = stat_at(staging.fd(), OsStr::new(id))?.id;
        match rename_no_replace(staging.fd(), OsStr::new(id), target.fd(), name) {
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
        self.drop_staged(&effect.id)?;
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
        let displacing = match &effect.action {
            Action::WriteFile {
                at,
                replaces: Some(displaced),
            } => Some((displaced.clone(), Some(at))),
            Action::RemoveFile { displaced } => Some((displaced.clone(), None)),
            Action::RemoveDir { at, id } => Some((
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
            let differs = differs(quarantine.fd(), id, &displaced.entry, contents)?;
            hook(Step::Compared);
            if let Some(reason) = differs {
                return self.returned(&quarantine, effect, &displaced, reason, now, hook);
            }
            let outcome = match place {
                Some(at) if staged => {
                    let placed = match self.parent(at) {
                        Ok((target, name)) => {
                            self.place(&staging, &effect.id, &target, name, hook)?
                        }
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
                Some(at) => match self.written_at(at, contents)? {
                    Some(identity) => Outcome::Done {
                        identity: Some(identity),
                    },
                    None => failed(Reason::Overtaken),
                },
                None => Outcome::Done { identity: None },
            };
            self.drop_staged(&effect.id)?;
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
        self.drop_staged(&effect.id)?;
        Ok(failed(Reason::Interrupted))
    }

    fn finished(
        &self,
        effect: &Effect,
        contents: &mut dyn Contents,
    ) -> io::Result<Option<Outcome>> {
        let entry_at = |at: &Location| -> io::Result<Option<Option<Stat>>> {
            match self.parent(at) {
                Ok((parent, name)) => Ok(Some(lookup(parent.fd(), name)?)),
                Err(
                    Unresolved::Missing(_)
                    | Unresolved::NotAFolder(_)
                    | Unresolved::Replaced { .. },
                ) => Ok(None),
                Err(err) => Err(err.into()),
            }
        };
        let trashed = || -> io::Result<bool> {
            let trash = self.private(&[TRASH_DIR])?;
            Ok(lookup(trash.fd(), OsStr::new(&effect.id))?.is_some())
        };
        let identity = match &effect.action {
            Action::CreateDir { at } => match entry_at(at)? {
                Some(Some(stat)) if stat.file_type == FileType::Directory => Some(stat.id),
                _ => return Ok(None),
            },
            Action::WriteFile { at, replaces } => match self.written_at(at, contents)? {
                Some(identity) => Some(identity),
                None if replaces.is_some() && trashed()? => {
                    return Ok(Some(failed(Reason::Overtaken)));
                }
                None => return Ok(None),
            },
            Action::Move { to, id, .. } => match entry_at(to)? {
                Some(Some(stat)) if stat.id == *id => None,
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

    fn written_at(&self, at: &Location, contents: &mut dyn Contents) -> io::Result<Option<FileId>> {
        let (parent, name) = match self.parent(at) {
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

    fn drop_staged(&self, id: &str) -> io::Result<()> {
        let staging = self.private(&[STAGING_DIR])?;
        remove_if_present(staging.fd(), id)
    }
}

fn differs(
    quarantine: BorrowedFd<'_>,
    id: &OsStr,
    expected: &Entry,
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
            let dir = rustix::fs::openat(quarantine, id, crate::DIR_FLAGS, Mode::empty())?;
            let mut held = Vec::new();
            for name in list(dir.as_fd())? {
                let platform = PLATFORM_FILES.iter().any(|file| name == OsStr::new(file));
                if !platform || stat_at(dir.as_fd(), &name)?.file_type != FileType::RegularFile {
                    held.push(name);
                }
            }
            Ok((!held.is_empty()).then_some(Reason::Holds(held)))
        }
    }
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
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    Ok(rustix::fs::openat(dir, name, flags, Mode::empty())?.into())
}

fn folder_entry(id: FileId) -> Entry {
    Entry {
        kind: EntryKind::Dir,
        id,
        size: 0,
        mtime_ns: 0,
        executable: false,
        linked: false,
    }
}

fn location_name(at: &Location) -> &OsStr {
    match at {
        Location::Parked(name) | Location::At { name, .. } => name,
    }
}
