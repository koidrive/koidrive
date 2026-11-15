// SPDX-License-Identifier: Apache-2.0

//! A device's synced folder in memory: entries keyed by inode, the parked directory, and the disk
//! side of every effect (docs/design/sync-engine.md sections 2.5, 5.2 and 5.3). An effect checks
//! the state it expects, sends the entry it displaces to quarantine, gives the new entry its
//! name, and sends the quarantined entry to the local trash. A crash can stop it between these
//! steps. Recovery then decides from the target, the staged download and the quarantine, as
//! `koidrive_fs::Section::recover` does, and the disk keeps no record of the effects it has done.
//!
//! Two things differ from the fs driver. The disk checks a displaced entry before it moves it to
//! quarantine, so a quarantined entry is never returned to its name. And it refuses to remove a
//! folder that has entries before it moves the folder, so a folder with entries is never in
//! quarantine.

use std::collections::{BTreeMap, BTreeSet};

use koidrive_engine::effects::{Base, Displaced, Effect, EffectRecord, Location};
use koidrive_engine::tree::{Content, FileIdentity, NodeId, same_content};

/// Where an entry sits.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Place {
    Root,
    /// Inside the directory with this inode.
    In(u128),
    /// In the parked directory, under this name.
    Parked(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub place: Place,
    pub name: String,
    /// The file's content, or `None` for a directory.
    pub content: Option<Content>,
    /// The manifest digest of every content this file has held, so that a user overwrite never
    /// brings back bytes the file held before.
    pub held: Vec<Vec<u8>>,
    pub mark: Option<Mark>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Mark {
    pub node: NodeId,
    pub ino: u128,
}

/// Why the disk did not do an effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// An entry the effect names is gone, or is another entry now.
    Moved,
    /// The file an effect displaces holds other content.
    Changed,
    /// The target name is taken.
    Taken,
    /// The directory to remove has entries.
    NotEmpty,
    /// The target is inside the directory that moves.
    Cycle,
    /// Recovery found that the effect stopped before it displaced anything, or that its target
    /// is not in the effect's final state any more.
    Interrupted,
    /// The effect sent the entry it replaced to the local trash, and its target does not hold the
    /// new content: the user changed or removed the written file, or took its name first.
    Overtaken,
}

/// How far an effect gets on the disk before the device crashes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cut {
    /// The entry it displaces is in quarantine, its download is staged, and the new entry has no
    /// name yet. An effect that neither displaces nor downloads has changed nothing.
    Quarantined,
    /// The new entry has its name, or found it taken. A displaced entry is still in quarantine.
    Placed,
    /// The effect is complete.
    Done,
}

/// What an effect, or the recovery of one, did to the disk.
#[derive(Clone, Debug, PartialEq)]
pub struct Applied {
    /// The identity of the entry the effect created, or why the disk did not do the effect.
    pub outcome: Result<Option<FileIdentity>, Refused>,
    /// The content of the file the effect sent to the local trash.
    pub trashed: Option<Content>,
}

pub fn identity(ino: u128) -> FileIdentity {
    FileIdentity {
        dev: 1,
        ino,
        birth_ns: Some(1_700_000_000_000_000_000),
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Disk {
    entries: BTreeMap<u128, Entry>,
    inos: u128,
    /// The effects in flight whose download is staged and has no name in the folder yet.
    pub(super) staged: BTreeSet<String>,
    /// The entries that effects in flight displaced and have not sent to the local trash yet.
    pub(super) quarantine: BTreeMap<String, (u128, Entry)>,
    /// The effects whose displaced entry is in the local trash.
    trash: BTreeSet<String>,
    binned: BTreeSet<i64>,
    marker: u64,
    pub(super) brought_back: BTreeSet<u128>,
}

impl Disk {
    pub fn marker(&self) -> u64 {
        self.marker
    }

    pub fn entries(&self) -> &BTreeMap<u128, Entry> {
        &self.entries
    }

    pub fn children<'a>(&'a self, place: &'a Place) -> impl Iterator<Item = u128> + 'a {
        self.entries
            .iter()
            .filter(move |(_, entry)| entry.place == *place)
            .map(|(ino, _)| *ino)
    }

    pub fn child(&self, place: &Place, name: &str) -> Option<u128> {
        self.children(place)
            .find(|ino| self.entries[ino].name == name)
    }

    /// The names from the root to the entry, or `None` for an entry in the parked directory.
    pub fn path(&self, ino: u128) -> Option<Vec<String>> {
        let mut names = Vec::new();
        let mut current = ino;
        loop {
            let entry = self.entries.get(&current)?;
            names.push(entry.name.clone());
            match &entry.place {
                Place::Root => break,
                Place::In(parent) => current = *parent,
                Place::Parked(_) => return None,
            }
        }
        names.reverse();
        Some(names)
    }

    /// Moves every entry of the parked directory and of quarantine to the root under a recovered
    /// name, as `Space::bring_back_all` does, and drops what is staged: `name` gives a parked
    /// entry's name, and an entry in quarantine keeps its own.
    pub fn bring_back_all(&mut self, name: impl Fn(&str) -> String) {
        self.staged.clear();
        let parked: Vec<(u128, String)> = (self.entries.iter())
            .filter_map(|(ino, entry)| match &entry.place {
                Place::Parked(parked) => Some((*ino, parked.clone())),
                _ => None,
            })
            .collect();
        for (ino, parked) in parked {
            let free = self.recovered(&name(&parked));
            let entry = self.entries.get_mut(&ino).expect("listed");
            entry.place = Place::Root;
            entry.name = free;
            self.brought_back.insert(ino);
        }
        for (ino, entry) in std::mem::take(&mut self.quarantine).into_values() {
            let name = self.recovered(&entry.name);
            let entry = Entry {
                place: Place::Root,
                name,
                ..entry
            };
            self.entries.insert(ino, entry);
            self.brought_back.insert(ino);
        }
    }

    fn recovered(&self, original: &str) -> String {
        (1..)
            .map(|n| match n {
                1 => format!("{original} (recovered)"),
                n => format!("{original} (recovered {n})"),
            })
            .find(|candidate| self.child(&Place::Root, candidate).is_none())
            .expect("a free name")
    }

    /// The entries the user sees, which leaves out the parked directory, in inode order.
    pub fn visible(&self) -> Vec<u128> {
        self.entries
            .keys()
            .copied()
            .filter(|ino| self.path(*ino).is_some())
            .collect()
    }

    /// True when `ancestor` is `place` or holds it.
    pub fn inside(&self, place: &Place, ancestor: u128) -> bool {
        let mut current = place;
        while let Place::In(ino) = current {
            if *ino == ancestor {
                return true;
            }
            current = &self.entries[ino].place;
        }
        false
    }

    pub fn binned(&self, write: i64) -> bool {
        self.binned.contains(&write)
    }

    pub fn shows(&self, write: i64) -> bool {
        (self.entries.values())
            .any(|entry| entry.content.as_ref().is_some_and(|c| c.mtime_ns == write))
    }

    /// True when some file, parked, in quarantine or neither, holds the write with this tag.
    pub fn holds(&self, write: i64) -> bool {
        (self.entries.values())
            .chain(self.quarantine.values().map(|(_, entry)| entry))
            .any(|entry| entry.content.as_ref().is_some_and(|c| c.mtime_ns == write))
    }

    pub fn reads(&self, at: &Location, ino: u128, write: i64) -> bool {
        self.locate(at).is_ok_and(|(place, name)| {
            self.child(&place, &name) == Some(ino)
                && (self.entries[&ino].content.as_ref()).is_some_and(|c| c.mtime_ns == write)
        })
    }

    /// Creates an entry, unless the name is taken.
    pub fn create(&mut self, place: Place, name: &str, content: Option<Content>) -> Option<u128> {
        if self.child(&place, name).is_some() {
            return None;
        }
        self.inos += 1;
        let held = content.iter().map(|c| c.manifest_digest.clone()).collect();
        let entry = Entry {
            place,
            name: name.to_owned(),
            content,
            held,
            mark: None,
        };
        self.entries.insert(self.inos, entry);
        Some(self.inos)
    }

    pub fn copy(&mut self, ino: u128, place: Place, name: &str) -> Option<u128> {
        let source = self
            .entries
            .get(&ino)
            .filter(|entry| entry.content.is_some())?
            .clone();
        let copy = self.create(place, name, source.content)?;
        self.entries.get_mut(&copy).expect("created").mark = source.mark;
        Some(copy)
    }

    pub fn save(&mut self, ino: u128, keeps_mark: bool) -> Option<u128> {
        self.entries.get(&ino)?.content.as_ref()?;
        let mut saved = self.entries.remove(&ino).expect("checked above");
        if !keeps_mark {
            saved.mark = None;
        }
        self.inos += 1;
        self.entries.insert(self.inos, saved);
        Some(self.inos)
    }

    pub fn mark(&mut self, at: &Location, ino: u128, node: &str) {
        if self.found(at).is_ok_and(|found| found == Some(ino)) {
            self.marked(ino, node);
        }
    }

    pub fn mark_of(&self, at: &Location, ino: u128) -> Option<NodeId> {
        if !self.found(at).is_ok_and(|found| found == Some(ino)) {
            return None;
        }
        let mark = self.entries[&ino].mark.as_ref()?;
        (mark.ino == ino).then(|| mark.node.clone())
    }

    fn marked(&mut self, ino: u128, node: &str) {
        let mark = Mark {
            node: node.to_owned(),
            ino,
        };
        self.entries.get_mut(&ino).expect("an entry").mark = Some(mark);
    }

    /// Replaces a file's content in place and returns the content it held.
    pub fn overwrite(&mut self, ino: u128, content: Content) -> Option<Content> {
        let entry = self.entries.get_mut(&ino)?;
        entry.held.push(content.manifest_digest.clone());
        entry.content.replace(content)
    }

    /// Moves an entry, unless the name is taken or the target is inside the entry.
    pub fn rename(&mut self, ino: u128, place: Place, name: &str) -> Result<(), Refused> {
        if self.child(&place, name).is_some() {
            return Err(Refused::Taken);
        }
        if self.inside(&place, ino) {
            return Err(Refused::Cycle);
        }
        let entry = self.entries.get_mut(&ino).ok_or(Refused::Moved)?;
        entry.place = place;
        entry.name = name.to_owned();
        Ok(())
    }

    /// Deletes an entry with everything in it and returns the contents of the deleted files.
    pub fn delete(&mut self, ino: u128) -> Vec<Content> {
        let doomed: Vec<u128> = self
            .entries
            .iter()
            .filter(|(other, entry)| **other == ino || self.inside(&entry.place, ino))
            .map(|(other, _)| *other)
            .collect();
        doomed
            .iter()
            .filter_map(|ino| self.entries.remove(ino)?.content)
            .collect()
    }

    /// Does one effect the way section 5.2 asks, as far as `until`: every directory on the way
    /// has the identity the effect names, a displaced file has the identity and content the
    /// engine last saw, and no create or rename replaces an entry. A displaced entry goes to
    /// the local trash also when the new entry finds its name taken.
    pub fn apply(&mut self, record: &EffectRecord, until: Option<Cut>) -> Option<Applied> {
        self.marker = record.counter;
        let Some(until) = until else {
            self.staged.remove(&record.effect_id);
            return None;
        };
        let mut trashed = None;
        let outcome = self.change(record, until, &mut trashed);
        if until == Cut::Done {
            self.staged.remove(&record.effect_id);
        }
        let removal = matches!(
            record.effect,
            Effect::RemoveFile { .. } | Effect::RemoveDir { .. }
        );
        if until == Cut::Done && (outcome.is_ok() && removal || trashed.is_some()) {
            self.trash.insert(record.effect_id.clone());
        }
        self.binned
            .extend(trashed.as_ref().map(|content| content.mtime_ns));
        Some(Applied { outcome, trashed })
    }

    /// Stages the download of a write before the effect section that applies it.
    pub fn stage(&mut self, record: &EffectRecord) {
        if matches!(record.effect, Effect::WriteFile { .. }) {
            self.staged.insert(record.effect_id.clone());
        }
    }

    fn change(
        &mut self,
        record: &EffectRecord,
        until: Cut,
        trashed: &mut Option<Content>,
    ) -> Result<Option<FileIdentity>, Refused> {
        let id = &record.effect_id;
        match &record.effect {
            Effect::CreateDir { at } => {
                let (place, name) = self.locate(at)?;
                if until == Cut::Quarantined {
                    return Ok(None);
                }
                let ino = self.create(place, &name, None).ok_or(Refused::Taken)?;
                self.marked(ino, &record.node);
                Ok(Some(identity(ino)))
            }
            Effect::WriteFile {
                at,
                content,
                replaces,
            } => {
                self.staged.insert(id.clone());
                let displaced = replaces.as_ref().map(|d| self.displaced(d)).transpose()?;
                let (place, name) = self.locate(at)?;
                if let Some(ino) = displaced
                    && let Some(entry) = self.entries.remove(&ino)
                {
                    self.quarantine.insert(id.clone(), (ino, entry));
                }
                if until == Cut::Quarantined {
                    return Ok(None);
                }
                let ino = self.create(place, &name, Some(content.clone()));
                if let Some(ino) = ino {
                    self.staged.remove(id);
                    self.marked(ino, &record.node);
                }
                if until == Cut::Done {
                    *trashed = self
                        .quarantine
                        .remove(id)
                        .and_then(|(_, entry)| entry.content);
                }
                let refused = match displaced {
                    Some(_) if until == Cut::Done => Refused::Overtaken,
                    _ => Refused::Taken,
                };
                ino.map(|ino| Some(identity(ino))).ok_or(refused)
            }
            Effect::Move {
                from,
                identity: moved,
                to,
            } => {
                let ino = self.at(from)?.filter(|ino| *ino == moved.ino);
                let ino = ino.ok_or(Refused::Moved)?;
                let (place, name) = self.locate(to)?;
                if until == Cut::Quarantined {
                    return Ok(None);
                }
                self.rename(ino, place, &name)?;
                Ok(None)
            }
            Effect::RemoveFile { displaced } => {
                let ino = self.displaced(displaced)?;
                if let Some(entry) = self.entries.remove(&ino) {
                    self.quarantine.insert(id.clone(), (ino, entry));
                }
                if until == Cut::Done {
                    *trashed = self
                        .quarantine
                        .remove(id)
                        .and_then(|(_, entry)| entry.content);
                }
                Ok(None)
            }
            Effect::RemoveDir { at, identity: gone } => {
                let ino = self.at(at)?.filter(|ino| *ino == gone.ino);
                let ino = ino.ok_or(Refused::Moved)?;
                if self.entries[&ino].content.is_some() {
                    return Err(Refused::Changed);
                }
                if self.children(&Place::In(ino)).next().is_some() {
                    return Err(Refused::NotEmpty);
                }
                let entry = self.entries.remove(&ino);
                if let Some(entry) = entry.filter(|_| until != Cut::Done) {
                    self.quarantine.insert(id.clone(), (ino, entry));
                }
                Ok(None)
            }
        }
    }

    /// Ends the effect in flight after a crash (section 5.3). An entry in quarantine means that
    /// the effect displaced it: the staged download gets its name if it has none yet, and the
    /// entry goes to the local trash. With nothing in quarantine and nothing staged, the effect
    /// is done when its target is in the effect's final state, which for a removal also takes its
    /// entry in the local trash. A removal whose name is empty without that entry fails, since the
    /// user moved or deleted the entry before the effect reached it. A write whose replaced entry
    /// goes or went to the local trash while its target does not hold the new content is
    /// overtaken. Anything else is an effect that stopped early or whose target the user has
    /// changed since.
    pub fn recover(&mut self, record: &EffectRecord) -> Applied {
        self.marker = record.counter;
        let staged = self.staged.remove(&record.effect_id);
        let Some((_, entry)) = self.quarantine.remove(&record.effect_id) else {
            let outcome = match staged {
                false => self.finished(record),
                true => Err(Refused::Interrupted),
            };
            return Applied {
                outcome,
                trashed: None,
            };
        };
        let outcome = match &record.effect {
            Effect::WriteFile { at, content, .. } if staged => {
                let placed = (self.locate(at).ok())
                    .and_then(|(place, name)| self.create(place, &name, Some(content.clone())));
                if let Some(ino) = placed {
                    self.marked(ino, &record.node);
                }
                placed
                    .map(|ino| Some(identity(ino)))
                    .ok_or(Refused::Overtaken)
            }
            Effect::WriteFile { at, content, .. } => {
                (self.written(at, content).map(Some)).ok_or(Refused::Overtaken)
            }
            _ => Ok(None),
        };
        self.trash.insert(record.effect_id.clone());
        self.binned
            .extend(entry.content.as_ref().map(|content| content.mtime_ns));
        Applied {
            outcome,
            trashed: entry.content,
        }
    }

    /// The identity of the entry an effect made, when the disk is in the effect's final state. A
    /// removal is final only once its entry is in the local trash.
    fn finished(&self, record: &EffectRecord) -> Result<Option<FileIdentity>, Refused> {
        let at = |at: &Location| self.found(at).map_err(|_| Refused::Interrupted);
        let done = match &record.effect {
            Effect::CreateDir { at: place } => at(place)?
                .filter(|ino| self.entries[ino].content.is_none())
                .map(|ino| Some(identity(ino))),
            Effect::WriteFile { at, content, .. } => match self.written(at, content) {
                None if self.trash.contains(&record.effect_id) => return Err(Refused::Overtaken),
                written => written.map(Some),
            },
            Effect::Move {
                identity: moved,
                to,
                ..
            } => (at(to)? == Some(moved.ino)).then_some(None),
            Effect::RemoveFile { .. } | Effect::RemoveDir { .. }
                if self.trash.contains(&record.effect_id) =>
            {
                Some(None)
            }
            Effect::RemoveFile {
                displaced: Displaced { at: place, .. },
            }
            | Effect::RemoveDir { at: place, .. } => match at(place)? {
                None => return Err(Refused::Moved),
                Some(_) => None,
            },
        };
        done.ok_or(Refused::Interrupted)
    }

    /// The identity of the file at `at`, when it holds `content`.
    fn written(&self, at: &Location, content: &Content) -> Option<FileIdentity> {
        let ino = self.found(at).ok()??;
        let held = self.entries[&ino].content.as_ref()?;
        same_content(held, content).then(|| identity(ino))
    }

    /// The file an effect displaces, when it still is the file the engine saw.
    fn displaced(&self, displaced: &Displaced) -> Result<u128, Refused> {
        let ino = self
            .at(&displaced.at)?
            .filter(|ino| *ino == displaced.identity.ino);
        let ino = ino.ok_or(Refused::Moved)?;
        match &self.entries[&ino].content {
            Some(content) if same_content(content, &displaced.content) => Ok(ino),
            _ => Err(Refused::Changed),
        }
    }

    /// The entry at a location, if one is there.
    fn at(&self, at: &Location) -> Result<Option<u128>, Refused> {
        let (place, name) = self.locate(at)?;
        Ok(match place {
            Place::Parked(_) => self.children(&place).next(),
            place => self.child(&place, &name),
        })
    }

    fn found(&self, at: &Location) -> Result<Option<u128>, Refused> {
        let Location::At {
            base: Base::Root,
            dirs,
            name,
        } = at
        else {
            return self.at(at);
        };
        let Some(dir) = dirs.last() else {
            return self.at(at);
        };
        let folder = Place::In(dir.identity.ino);
        match self.entries.get(&dir.identity.ino) {
            Some(entry) if entry.content.is_none() && self.path(dir.identity.ino).is_some() => {
                Ok(self.child(&folder, name))
            }
            _ => Err(Refused::Moved),
        }
    }

    /// The directory a location is in and the name it has there. An entry in the parked directory
    /// is named after the node it is parked as.
    fn locate(&self, at: &Location) -> Result<(Place, String), Refused> {
        let (base, dirs, name) = match at {
            Location::Parked(node) => return Ok((Place::Parked(node.clone()), node.clone())),
            Location::At { base, dirs, name } => (base, dirs, name),
        };
        let mut place = match base {
            Base::Root => Place::Root,
            Base::Parked { node, identity } => {
                let parked = self.children(&Place::Parked(node.clone())).next();
                let ino = parked.filter(|ino| *ino == identity.ino);
                Place::In(ino.ok_or(Refused::Moved)?)
            }
        };
        for dir in dirs {
            let ino = self.child(&place, &dir.name);
            let ino =
                ino.filter(|ino| *ino == dir.identity.ino && self.entries[ino].content.is_none());
            place = Place::In(ino.ok_or(Refused::Moved)?);
        }
        Ok((place, name.clone()))
    }
}
