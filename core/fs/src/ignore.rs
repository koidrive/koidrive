// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Read};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rustix::fs::{Dir, FileType, Mode, OFlags};

use koidrive_ignore::Source;

use crate::git::Spelled;
use crate::{
    Cut, Entry, EntryKind, FileId, GitIndex, Stat, is_gone, open_attached, open_dir_at, stat_at,
    stat_fd,
};

const MAX_RULES: u64 = 1 << 20;

/// Why a path is left out of scans.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ignored {
    No,
    /// A platform file such as `.DS_Store`. No rule can bring it back.
    BuiltIn,
    /// A rule of a rules file or the device's own list.
    Rule,
}

/// The matcher scans ask. The driver implements it over
/// `koidrive_ignore::Matcher`.
pub trait Ignore {
    /// The rules file of kind `source` in folder `dir` (relative to the
    /// root, empty for the root) holds `rules`, or the folder has none.
    fn set_rules(&mut self, dir: &Path, source: Source, rules: Option<&[u8]>);

    /// Whether the entry at `path`, relative to the root, is left out. A path
    /// below a left-out folder is left out.
    fn status(&self, path: &Path, kind: EntryKind) -> Ignored;

    /// The form in which the matcher compares `name`. Names with the same key
    /// are one name to it: it holds the rules of a folder under the keys of
    /// the folder's path, and a file whose name has the key of a
    /// [`Source::file_name`] is a rules file of that kind.
    fn key(&self, name: &OsStr) -> OsString;

    /// Whether rules files of kind `source` apply. Scans read only those.
    fn follows(&self, _source: Source) -> bool {
        true
    }

    /// The git index of the work tree at `tree` (relative to the root, empty for the root) lists the files at
    /// `paths`, each relative to the work tree with names joined by `/` (RFC 0033 §4).
    fn set_tracked(&mut self, _tree: &Path, _paths: &[String]) {}
}

type Held = (PathBuf, Vec<u8>);

/// The rules of one root and what its scans have seen of them.
///
/// A new `Rules` knows no rules file. A scan reads the rules files of each
/// folder it lists, so scan the whole tree before any part of it.
pub struct Rules {
    ignore: Box<dyn Ignore + Send>,
    rules_files: Vec<(Source, OsString)>,
    files: BTreeMap<(PathBuf, Source), Held>,
    ignored: BTreeMap<PathBuf, BTreeSet<OsString>>,
    indexes: BTreeMap<PathBuf, GitIndex<Spelled>>,
    tracked: BTreeMap<PathBuf, Vec<String>>,
    budget: usize,
    search: Search,
}

#[derive(Default)]
struct Search {
    walks: u64,
    walk: Option<Walk>,
    pending: HashMap<Key, u64>,
    decided: HashMap<Key, Decided>,
    held: BTreeMap<PathBuf, Entry>,
    sought: HashSet<Key>,
    postponed: BTreeSet<PathBuf>,
    scanned: Vec<PathBuf>,
    reseeded: Vec<PathBuf>,
    coverage: Coverage,
}

/// How much of the left-out entries the last search that decided anything looked at.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Coverage {
    /// The search ended before it had looked everywhere ([`Rules::end_search`]).
    pub ended_early: bool,
    /// The left-out folders and entries the search could not open or stat.
    pub unopened: BTreeSet<PathBuf>,
    /// The left-out folders and entries that kept being replaced or moved while the search walked them, after it
    /// had started over as often as it allows in one search. The search went on without them and did not walk them
    /// to the end later. Also each folder queued by a last-round check that no walk finished, since such a folder
    /// owes coverage until a walk finishes it.
    pub replaced: BTreeSet<PathBuf>,
}

impl Coverage {
    /// Whether the search looked everywhere, so that an entry it did not find is not on the disk.
    pub fn everywhere(&self) -> bool {
        !self.ended_early && self.unopened.is_empty() && self.replaced.is_empty()
    }
}

impl Search {
    fn prune(&mut self) {
        let keys: HashSet<Key> = (self.held.values())
            .map(|entry| (entry.id, entry.kind))
            .chain(self.sought.iter().copied())
            .collect();
        self.pending.retain(|key, _| keys.contains(key));
        self.decided.retain(|key, _| keys.contains(key));
        if self.pending.is_empty() {
            self.walk = None;
        }
    }
}

enum Decided {
    Found(PathBuf),
    Missed,
    Unproven,
}

pub(crate) struct Seeded {
    scanned: Vec<PathBuf>,
    reseeded: Vec<PathBuf>,
}

impl Seeded {
    pub(crate) fn unscanned(&self, path: &Path) -> bool {
        let under = |paths: &[PathBuf]| paths.iter().any(|scope| path.starts_with(scope));
        !under(&self.scanned) || under(&self.reseeded)
    }
}

struct Listing {
    dir: PathBuf,
    names: Vec<OsString>,
    chain: Option<Vec<FileId>>,
}

fn same_chain(before: &[FileId], after: &[FileId]) -> bool {
    before.len() == after.len() && before.iter().zip(after).all(|(a, b)| a.same(b))
}

struct Walk {
    listings: Vec<Listing>,
    base: Option<(PathBuf, OwnedFd)>,
    folders: Vec<Folder>,
    reach: Option<(usize, Option<OwnedFd>)>,
    bases: Vec<(PathBuf, Vec<FileId>)>,
    restarts: usize,
    relisted: HashSet<PathBuf>,
    passed: BTreeMap<PathBuf, Seen>,
    check: Vec<PathBuf>,
    rounds: usize,
    changed: bool,
    owed: BTreeSet<PathBuf>,
    unopened: BTreeSet<PathBuf>,
    replaced: BTreeSet<PathBuf>,
}

struct Seen {
    id: Option<FileId>,
    stamp: (i128, i128),
    racy: bool,
    listing: u64,
    needs_walk: bool,
    done: bool,
}

impl Seen {
    fn is(&self, id: &FileId) -> bool {
        self.id.is_some_and(|own| own.same(id))
    }

    fn of(
        fd: BorrowedFd<'_>,
        dir: &Path,
        unopened: &mut BTreeSet<PathBuf>,
    ) -> Option<(Self, Vec<OsString>)> {
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos() as i128);
        let Ok(stat) = stat_fd(fd) else {
            unopened.insert(dir.to_path_buf());
            return None;
        };
        let stamp = (stat.mtime_ns, stat.ctime_ns);
        let names = listed(fd, dir, unopened)?;
        let seen = Self {
            id: Some(stat.id),
            stamp,
            racy: stamp.0.max(stamp.1) > at - 1_000_000_000,
            listing: digest(names.iter().map(|(name, ino)| (name.as_os_str(), *ino))),
            needs_walk: false,
            done: false,
        };
        Some((seen, names.into_iter().map(|(name, _)| name).collect()))
    }
}

fn digest<'a>(names: impl IntoIterator<Item = (&'a OsStr, u64)>) -> u64 {
    names.into_iter().fold(0, |sum, (name, ino)| {
        let mut entry = DefaultHasher::new();
        (name, ino).hash(&mut entry);
        sum.wrapping_add(entry.finish())
    })
}

#[cfg(target_os = "macos")]
fn gone(root: BorrowedFd<'_>, folder: &Folder) -> bool {
    matches!(path_of(root, folder.id.ino), Ok(None))
}

#[cfg(not(target_os = "macos"))]
fn gone(_: BorrowedFd<'_>, folder: &Folder) -> bool {
    (folder.fd.as_ref()).is_some_and(|fd| stat_fd(fd.as_fd()).is_ok_and(|stat| stat.nlink == 0))
}

struct Folder {
    dir: PathBuf,
    id: FileId,
    names: Vec<OsString>,
    next: usize,
    fd: Option<OwnedFd>,
}

enum Replaced {
    Folder(usize, Option<OwnedFd>),
    Path(PathBuf),
}

fn listed(
    fd: BorrowedFd<'_>,
    dir: &Path,
    unopened: &mut BTreeSet<PathBuf>,
) -> Option<Vec<(OsString, u64)>> {
    let Ok(mut entries) = Dir::read_from(fd) else {
        unopened.insert(dir.to_path_buf());
        return None;
    };
    let mut names: Vec<(OsString, u64)> = Vec::new();
    for item in std::iter::from_fn(|| entries.read()) {
        let Ok(item) = item else {
            unopened.insert(dir.to_path_buf());
            break;
        };
        let name = OsStr::from_bytes(item.file_name().to_bytes()).to_os_string();
        names.push((name, item.ino()));
    }
    names.retain(|(name, _)| *name != "." && *name != "..");
    names.sort_unstable();
    Some(names)
}

impl Walk {
    fn new(ignored: &BTreeMap<PathBuf, BTreeSet<OsString>>, restarts: usize) -> Self {
        Self {
            listings: (ignored.iter())
                .map(|(dir, names)| Listing {
                    dir: dir.clone(),
                    names: names.iter().rev().cloned().collect(),
                    chain: None,
                })
                .rev()
                .collect(),
            base: None,
            folders: Vec::new(),
            reach: None,
            bases: Vec::new(),
            restarts,
            relisted: HashSet::new(),
            passed: BTreeMap::new(),
            check: Vec::new(),
            rounds: 0,
            changed: false,
            owed: BTreeSet::new(),
            unopened: BTreeSet::new(),
            replaced: BTreeSet::new(),
        }
    }

    fn above(&self, at: usize) -> Option<BorrowedFd<'_>> {
        match at.checked_sub(1) {
            None => self.base.as_ref().map(|(_, fd)| fd.as_fd()),
            Some(parent) => self.folders[parent].fd.as_ref().map(|fd| fd.as_fd()),
        }
    }

    fn push(&mut self, folder: Folder) {
        self.folders.push(folder);
        if let Some(lowest) = self.folders.len().checked_sub(OPEN_FOLDERS + 1) {
            self.folders[lowest].fd = None;
        }
    }

    fn complete(&self) -> bool {
        self.folders.last().is_some_and(|folder| {
            folder.next == folder.names.len() && (folder.fd.is_some() || self.folders.len() == 1)
        })
    }

    fn finish(&mut self) {
        let Some(done) = self.folders.pop() else {
            return;
        };
        self.replaced.remove(&done.dir);
        self.owed.remove(&done.dir);
        if let Some(seen) = self.passed.get_mut(&done.dir) {
            seen.done = true;
        }
    }

    fn quiet(&mut self, root: BorrowedFd<'_>, at: usize, cost: &mut usize, limit: usize) -> bool {
        let replaced = self.folders[at].dir.clone();
        let old = u64::try_from(self.folders[at].id.ino).unwrap_or_default();
        let (Some(up), Some(child)) = (replaced.parent(), replaced.file_name()) else {
            return false;
        };
        let checked: Vec<PathBuf> = (self.passed.keys())
            .take_while(|dir| self.check.last().is_none_or(|next| *dir < next))
            .filter(|dir| !dir.starts_with(&replaced))
            .cloned()
            .collect();
        let mut above = Ancestors::default();
        for dir in checked {
            if *cost >= limit {
                return false;
            }
            let (Some(parent), Some(name)) = (dir.parent(), dir.file_name()) else {
                return false;
            };
            *cost += 1;
            let mut stale = None;
            let opened = above.open(root, parent, &self.passed, cost, &mut stale);
            let Some(seen) = self.passed.get_mut(&dir) else {
                return false;
            };
            let stat = match opened {
                Ok(true) if stale.is_none() => stat_at(above.top(root), name),
                Ok(false) if stale.is_none() => Err(io::ErrorKind::NotFound.into()),
                _ => return false,
            };
            match stat {
                Err(err) if is_gone(&err) && seen.needs_walk => continue,
                Ok(stat) if !seen.needs_walk && seen.is(&stat.id) => {
                    if (stat.mtime_ns, stat.ctime_ns) == seen.stamp && !seen.racy {
                        continue;
                    }
                }
                _ => return false,
            }
            *cost += 1;
            let Ok(fd) = open_dir_at(above.top(root), name) else {
                return false;
            };
            let mut failed = BTreeSet::new();
            let Some((now, _)) = Seen::of(fd.as_fd(), &dir, &mut failed) else {
                return false;
            };
            if !failed.is_empty() {
                return false;
            }
            let mut expected = seen.listing;
            if dir.as_path() == up {
                expected = expected.wrapping_sub(digest([(child, old)]));
                if let Ok(stat) = stat_at(fd.as_fd(), child) {
                    let ino = u64::try_from(stat.id.ino).unwrap_or_default();
                    expected = expected.wrapping_add(digest([(child, ino)]));
                }
            }
            if !now.id.is_some_and(|id| seen.is(&id)) || now.listing != expected {
                return false;
            }
            *seen = Seen {
                done: seen.done,
                ..now
            };
        }
        above.pop_to(root, 0, cost).is_none()
    }

    fn leave(&mut self, replaced: Replaced) {
        let (at, through) = match replaced {
            Replaced::Folder(at, through) => (at, through),
            Replaced::Path(path) => {
                self.replaced.insert(path);
                return;
            }
        };
        self.replaced.insert(self.folders[at].dir.clone());
        self.folders.truncate(at);
        self.reach = None;
        let Some(parent) = self.folders.last_mut() else {
            return;
        };
        if through.is_some() {
            parent.fd = through;
        }
        if !self.relisted.insert(parent.dir.clone()) {
            return;
        }
        let Some((now, names)) = (parent.fd.as_ref())
            .and_then(|fd| Seen::of(fd.as_fd(), &parent.dir, &mut self.unopened))
        else {
            return;
        };
        let added: Vec<OsString> = (names.into_iter())
            .filter(|name| parent.names.binary_search(name).is_err())
            .collect();
        parent.names.extend(added);
        self.passed.insert(parent.dir.clone(), now);
    }

    fn recheck(
        &mut self,
        root: BorrowedFd<'_>,
        dir: PathBuf,
        above: &mut Ancestors,
        cut: &mut dyn FnMut(Cut<'_>),
    ) -> usize {
        let (true, Some(parent), Some(name)) = (
            self.passed.contains_key(&dir),
            dir.parent(),
            dir.file_name(),
        ) else {
            return 1;
        };
        cut(Cut::Confirming(&dir));
        let mut cost = 1;
        let mut stale = None;
        let attached = above.open(root, parent, &self.passed, &mut cost, &mut stale);
        if let Some(path) = stale {
            self.changed = true;
            self.owed.insert(path);
        }
        let Some(seen) = self.passed.get_mut(&dir) else {
            return cost;
        };
        match attached {
            Ok(true) => {}
            Ok(false) => {
                if !std::mem::replace(&mut seen.needs_walk, true) {
                    self.changed = true;
                    self.owed.insert(dir);
                }
                return cost;
            }
            Err(_) => {
                self.unopened.insert(dir);
                return cost;
            }
        }
        let stat = match stat_at(above.top(root), name) {
            Ok(stat) => stat,
            Err(err) if is_gone(&err) => {
                if !std::mem::replace(&mut seen.needs_walk, true) {
                    self.changed = true;
                    self.owed.insert(dir);
                }
                return cost;
            }
            Err(_) => {
                self.unopened.insert(dir);
                return cost;
            }
        };
        if !seen.needs_walk && seen.done && seen.is(&stat.id) {
            if (stat.mtime_ns, stat.ctime_ns) == seen.stamp && !seen.racy {
                return cost;
            }
            cut(Cut::OpeningDir(&dir));
            cost += 1;
            match open_dir_at(above.top(root), name) {
                Ok(fd) => {
                    let Some((now, _)) = Seen::of(fd.as_fd(), &dir, &mut self.unopened) else {
                        return cost;
                    };
                    if now.id.is_some_and(|id| seen.is(&id)) && now.listing == seen.listing {
                        *seen = Seen { done: true, ..now };
                        return cost;
                    }
                }
                Err(err) if is_gone(&err) => {
                    if !std::mem::replace(&mut seen.needs_walk, true) {
                        self.changed = true;
                        self.owed.insert(dir);
                    }
                    return cost;
                }
                Err(_) => {
                    self.unopened.insert(dir);
                    return cost;
                }
            }
        }
        seen.needs_walk = true;
        if seen.id.is_some() {
            self.owed.insert(dir.clone());
        }
        self.changed = true;
        self.listings.push(Listing {
            dir: parent.to_path_buf(),
            names: vec![name.to_os_string()],
            chain: Some(above.chain()),
        });
        cost
    }
}

struct Ancestor {
    name: OsString,
    id: FileId,
    fd: Option<OwnedFd>,
}

#[derive(Default)]
struct Ancestors(Vec<Ancestor>);

impl Ancestors {
    fn top<'a>(&'a self, root: BorrowedFd<'a>) -> BorrowedFd<'a> {
        (self.0.last())
            .and_then(|ancestor| ancestor.fd.as_ref())
            .map_or(root, |fd| fd.as_fd())
    }

    fn path(&self, len: usize) -> PathBuf {
        self.0[..len]
            .iter()
            .map(|ancestor| &ancestor.name)
            .collect()
    }

    fn chain(&self) -> Vec<FileId> {
        self.0.iter().map(|ancestor| ancestor.id).collect()
    }

    fn open(
        &mut self,
        root: BorrowedFd<'_>,
        dir: &Path,
        passed: &BTreeMap<PathBuf, Seen>,
        opens: &mut usize,
        stale: &mut Option<PathBuf>,
    ) -> io::Result<bool> {
        let names: Vec<&OsStr> = dir.components().map(|name| name.as_os_str()).collect();
        let keep = (self.0.iter().zip(&names))
            .take_while(|(ancestor, name)| ancestor.name == **name)
            .count();
        if let Some(path) = self.pop_to(root, keep, opens) {
            stale.get_or_insert(path);
        }
        for name in names.into_iter().skip(self.0.len()) {
            *opens += 1;
            let fd = match open_dir_at(self.top(root), name) {
                Ok(fd) => fd,
                Err(err) if is_gone(&err) => return Ok(false),
                Err(err) => return Err(err),
            };
            let id = stat_fd(fd.as_fd())?.id;
            if self.0.len() >= OPEN_FOLDERS
                && let Some(last) = self.0.last_mut()
            {
                last.fd = None;
            }
            self.0.push(Ancestor {
                name: name.to_os_string(),
                id,
                fd: Some(fd),
            });
            let at = self.path(self.0.len());
            if stale.is_none()
                && passed
                    .get(&at)
                    .is_some_and(|seen| seen.id.is_some() && !seen.is(&id))
            {
                *stale = Some(at);
            }
        }
        Ok(true)
    }

    fn pop_to(&mut self, root: BorrowedFd<'_>, keep: usize, opens: &mut usize) -> Option<PathBuf> {
        if keep >= self.0.len() {
            return None;
        }
        let top = match self.reach(root, keep, opens) {
            Ok(top) => top,
            Err(stale) => return Some(stale),
        };
        let mut stale = None;
        let mut above = top.try_clone().ok();
        for (at, ancestor) in self.0.iter().enumerate().skip(keep) {
            above = above
                .filter(|fd| {
                    stat_at(fd.as_fd(), &ancestor.name).is_ok_and(|stat| stat.id.same(&ancestor.id))
                })
                .and_then(|fd| match &ancestor.fd {
                    Some(held) => held.try_clone().ok(),
                    None => {
                        *opens += 1;
                        ancestor.open_in(fd.as_fd())
                    }
                });
            if above.is_none() {
                stale = Some(self.path(at + 1));
                break;
            }
        }
        self.0.truncate(keep);
        if let Some(last) = self.0.last_mut() {
            last.fd.get_or_insert(top);
        }
        stale
    }

    fn reach(
        &mut self,
        root: BorrowedFd<'_>,
        len: usize,
        opens: &mut usize,
    ) -> Result<OwnedFd, PathBuf> {
        let held = self.0[..len]
            .iter()
            .rposition(|ancestor| ancestor.fd.is_some());
        let fd = match held {
            Some(at) => self.0[at].fd.as_ref().map(|fd| fd.try_clone()),
            None => Some(root.try_clone_to_owned()),
        };
        let Some(Ok(mut fd)) = fd else {
            self.0.clear();
            return Err(PathBuf::new());
        };
        for at in held.map_or(0, |at| at + 1)..len {
            *opens += 1;
            let Some(next) = self.0[at].open_in(fd.as_fd()) else {
                let stale = self.path(at + 1);
                self.0.truncate(at);
                if let Some(last) = self.0.last_mut() {
                    last.fd.get_or_insert(fd);
                }
                return Err(stale);
            };
            fd = next;
        }
        Ok(fd)
    }
}

impl Ancestor {
    fn open_in(&self, above: BorrowedFd<'_>) -> Option<OwnedFd> {
        open_dir_at(above, &self.name)
            .ok()
            .filter(|fd| stat_fd(fd.as_fd()).is_ok_and(|stat| stat.id.same(&self.id)))
    }
}

const OPEN_FOLDERS: usize = 16;

const RESTARTS: usize = 8;

const CHECKS: usize = 4;

const SEARCH_BUDGET: usize = 20_000;

impl Rules {
    pub fn new(ignore: Box<dyn Ignore + Send>) -> Self {
        Self {
            rules_files: (Source::ALL.into_iter())
                .filter(|source| ignore.follows(*source))
                .map(|source| (source, ignore.key(source.file_name().as_ref())))
                .collect(),
            ignore,
            files: BTreeMap::new(),
            ignored: BTreeMap::new(),
            indexes: BTreeMap::new(),
            tracked: BTreeMap::new(),
            budget: SEARCH_BUDGET,
            search: Search::default(),
        }
    }

    /// The same rules with a search that looks at `budget` left-out entries
    /// per rescan, and at least one.
    pub fn with_search_budget(mut self, budget: usize) -> Self {
        self.budget = budget.max(1);
        self
    }

    /// Ends the search under way before it has looked everywhere. Each entry it still looks for counts as not
    /// found without proof: the next scan of its path reports it as [`Change::Missing`](crate::Change::Missing).
    /// So does each entry a search misses after it could not open a left-out folder ([`Rules::coverage`]).
    pub fn end_search(&mut self) {
        let search = &mut self.search;
        let walk = search.walk.take();
        search.coverage = Coverage {
            ended_early: true,
            unopened: walk
                .as_ref()
                .map_or_else(BTreeSet::new, |walk| walk.unopened.clone()),
            replaced: walk.map_or_else(BTreeSet::new, |walk| walk.replaced),
        };
        for (key, _) in search.pending.drain() {
            search.decided.insert(key, Decided::Unproven);
        }
    }

    pub fn coverage(&self) -> &Coverage {
        &self.search.coverage
    }

    pub(crate) fn searching(&self) -> bool {
        let search = &self.search;
        (search.held.values()).any(|entry| search.pending.contains_key(&(entry.id, entry.kind)))
    }

    pub(crate) fn held(&self) -> Option<&BTreeMap<PathBuf, Entry>> {
        Some(&self.search.held).filter(|held| !held.is_empty())
    }

    pub(crate) fn forget(&mut self, scope: &Path) {
        let search = &mut self.search;
        search.held.retain(|path, _| !path.starts_with(scope));
        search.postponed.retain(|path| !path.starts_with(scope));
        search.prune();
        search.reseeded.push(scope.to_path_buf());
        search.reseeded = crate::outermost(std::mem::take(&mut search.reseeded));
    }

    fn key(&self, path: &Path) -> PathBuf {
        path.iter().map(|name| self.ignore.key(name)).collect()
    }

    pub fn status(&self, path: &Path, kind: EntryKind) -> Ignored {
        if path.as_os_str().is_empty() {
            return Ignored::No;
        }
        self.ignore.status(path, kind)
    }

    pub(crate) fn work_trees(&self) -> impl Iterator<Item = (&PathBuf, Option<&Path>)> {
        (self.indexes.iter()).map(|(tree, index)| (tree, index.git_dir()))
    }

    pub(crate) fn read_index(&mut self, root: &Path, tree: &Path) -> Vec<(PathBuf, Vec<String>)> {
        if let Some(index) = self.indexes.get_mut(tree) {
            if !index.refresh() && self.tracked.contains_key(tree) {
                return Vec::new();
            }
            return self.publish(tree).into_iter().collect();
        }
        let Some(mut index) = GitIndex::find(&root.join(tree)) else {
            return Vec::new();
        };
        index.refresh();
        self.indexes.insert(tree.to_path_buf(), index);
        self.publish_above(tree)
    }

    pub(crate) fn tracked(&self, tree: &Path) -> &[String] {
        self.tracked.get(tree).map_or(&[], Vec::as_slice)
    }

    /// The scopes to scan again once the list of the work tree at `tree` changed from `before`: each file one of the
    /// lists holds, or the outermost folder above it that `known` holds and the rules leave out.
    pub(crate) fn rescope(
        &self,
        tree: &Path,
        before: &[String],
        known: impl Fn(&Path) -> bool,
    ) -> Vec<PathBuf> {
        let before: BTreeSet<&String> = before.iter().collect();
        let after: BTreeSet<&String> = self.tracked(tree).iter().collect();
        (before.symmetric_difference(&after))
            .map(|path| {
                let path = tree.join(path);
                let left_out = (path.ancestors().skip(1))
                    .filter(|folder| known(folder))
                    .filter(|folder| self.status(folder, EntryKind::Dir) != Ignored::No)
                    .last()
                    .map(Path::to_path_buf);
                left_out.unwrap_or(path)
            })
            .collect()
    }

    pub(crate) fn forget_index(&mut self, tree: &Path) -> Vec<(PathBuf, Vec<String>)> {
        let mut changed: Vec<_> = (self.set_tracked(tree, Vec::new()).into_iter())
            .map(|before| (tree.to_path_buf(), before))
            .collect();
        if self.indexes.remove(tree).is_some() {
            changed.extend(self.publish_above(tree));
        }
        changed
    }

    fn publish_above(&mut self, tree: &Path) -> Vec<(PathBuf, Vec<String>)> {
        let above: Vec<PathBuf> = (tree.ancestors())
            .filter(|folder| self.indexes.contains_key(*folder))
            .map(Path::to_path_buf)
            .collect();
        above.iter().filter_map(|tree| self.publish(tree)).collect()
    }

    fn publish(&mut self, tree: &Path) -> Option<(PathBuf, Vec<String>)> {
        let inner: Vec<PathBuf> = (self.indexes.keys())
            .filter_map(|folder| folder.strip_prefix(tree).ok())
            .filter(|folder| !folder.as_os_str().is_empty())
            .map(|folder| self.key(folder))
            .collect();
        let owned = |path: &&str| {
            inner.is_empty() || {
                let path = self.key(Path::new(path));
                !inner.iter().any(|folder| path.starts_with(folder))
            }
        };
        let index = self.indexes.get(tree)?;
        let mut listed: Vec<String> = index.listed().filter(owned).map(str::to_owned).collect();
        listed.sort_unstable();
        Some((tree.to_path_buf(), self.set_tracked(tree, listed)?))
    }

    fn set_tracked(&mut self, tree: &Path, listed: Vec<String>) -> Option<Vec<String>> {
        let before = self.tracked.get(tree).cloned().unwrap_or_default();
        if before == listed {
            return None;
        }
        let respelled = listed.is_empty()
            && (self.tracked.keys()).any(|live| live != tree && self.key(live) == self.key(tree));
        if !respelled {
            self.ignore.set_tracked(tree, &listed);
        }
        if listed.is_empty() {
            self.tracked.remove(tree);
        } else {
            self.tracked.insert(tree.to_path_buf(), listed);
        }
        Some(before)
    }

    fn set(&mut self, dir: &Path, source: Source, rules: Option<Vec<u8>>) -> Option<Held> {
        self.ignore.set_rules(dir, source, rules.as_deref());
        let key = (self.key(dir), source);
        match rules {
            Some(bytes) => self.files.insert(key, (dir.to_path_buf(), bytes)),
            None => self.files.remove(&key),
        }
    }
}

pub(crate) type Wanted = HashMap<Key, Option<PathBuf>>;

pub(crate) type Key = (FileId, EntryKind);

fn keys_of(entry: &Entry) -> impl Iterator<Item = Key> {
    [
        (entry.id, entry.kind),
        (entry.id.without_handle(), entry.kind),
    ]
    .into_iter()
}

pub(crate) struct Pending<'r> {
    rules: &'r mut Rules,
    root: &'r Path,
    before: BTreeMap<(PathBuf, Source), Option<Held>>,
    tracked: BTreeMap<PathBuf, Vec<String>>,
    listed: BTreeMap<PathBuf, BTreeSet<OsString>>,
    read: BTreeSet<PathBuf>,
}

impl<'r> Pending<'r> {
    pub(crate) fn new(rules: &'r mut Rules, root: &'r Path) -> Self {
        Self {
            rules,
            root,
            before: BTreeMap::new(),
            tracked: BTreeMap::new(),
            listed: BTreeMap::new(),
            read: BTreeSet::new(),
        }
    }

    pub(crate) fn status(&self, path: &Path, kind: EntryKind) -> Ignored {
        self.rules.status(path, kind)
    }

    pub(crate) fn key(&self, name: &OsStr) -> OsString {
        self.rules.ignore.key(name)
    }

    pub(crate) fn read(
        &mut self,
        dir: BorrowedFd<'_>,
        rel: &Path,
        names: &BTreeSet<OsString>,
    ) -> RulesRead {
        let dir_key = self.rules.key(rel);
        let mut reads = Vec::new();
        for (source, file_key) in &self.rules.rules_files {
            let files: Vec<&OsString> = (names.iter())
                .filter(|name| self.rules.ignore.key(name) == *file_key)
                .filter(|name| match stat_at(dir, name) {
                    Ok(stat) => stat.file_type == FileType::RegularFile,
                    Err(err) => !is_gone(&err),
                })
                .collect();
            if files.len() > 1 {
                return RulesRead::Unknown;
            }
            let key = (dir_key.clone(), *source);
            let read = files.first().map(|name| read_rules(dir, name));
            if matches!(read, Some(Err(_))) && !self.rules.files.contains_key(&key) {
                return RulesRead::Unknown;
            }
            reads.push((key, read));
        }
        self.listed.entry(rel.to_path_buf()).or_default();
        self.read.insert(rel.to_path_buf());
        let tracked = if names.contains(OsStr::new(".git")) {
            self.rules.read_index(self.root, rel)
        } else {
            self.rules.forget_index(rel)
        };
        let mut changed = !tracked.is_empty();
        for (tree, before) in tracked {
            self.tracked.entry(tree).or_insert(before);
        }
        for (key, read) in reads {
            let found = match read {
                Some(Ok(found)) => found,
                Some(Err(_)) => continue,
                None => None,
            };
            let held = self.rules.files.get(&key);
            let same = found.as_ref() == held.map(|(_, bytes)| bytes);
            if same && held.is_none_or(|(dir, _)| dir == rel) {
                continue;
            }
            let before = self.rules.set(rel, key.1, found);
            self.before.entry(key).or_insert(before);
            changed |= !same;
        }
        if changed {
            RulesRead::Changed
        } else {
            RulesRead::Same
        }
    }

    pub(crate) fn leaves_out(&mut self, rel: &Path, kind: EntryKind) -> bool {
        match self.rules.status(rel, kind) {
            Ignored::No => false,
            Ignored::BuiltIn => true,
            Ignored::Rule => {
                if let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) {
                    self.listed
                        .entry(parent.to_path_buf())
                        .or_default()
                        .insert(name.to_os_string());
                }
                true
            }
        }
    }

    pub(crate) fn undo(self) {
        for (tree, before) in self.tracked {
            self.rules.indexes.remove(&tree);
            self.rules.set_tracked(&tree, before);
        }
        for (key, before) in self.before {
            let now = self.rules.files.get(&key).map(|(dir, _)| dir.clone());
            let source = key.1;
            match (before, now) {
                (Some((dir, bytes)), _) => self.rules.set(&dir, source, Some(bytes)),
                (None, Some(dir)) => self.rules.set(&dir, source, None),
                (None, None) => None,
            };
        }
    }

    pub(crate) fn settle(&mut self) {
        for (folder, names) in std::mem::take(&mut self.listed) {
            if let Some(Walk { listings: walk, .. }) = &mut self.rules.search.walk {
                let before = self.rules.ignored.get(&folder);
                let added: Vec<OsString> = (names.iter().rev())
                    .filter(|name| !before.is_some_and(|before| before.contains(*name)))
                    .cloned()
                    .collect();
                if !added.is_empty() {
                    walk.push(Listing {
                        dir: folder.clone(),
                        names: added,
                        chain: None,
                    });
                }
            }
            if names.is_empty() {
                self.rules.ignored.remove(&folder);
            } else {
                self.rules.ignored.insert(folder, names);
            }
        }
    }

    pub(crate) fn find(&mut self, root: BorrowedFd<'_>, wanted: &mut Wanted) {
        let mut missing = wanted.len();
        self.rules.ignored.retain(|folder, names| {
            if missing == 0 {
                return true;
            }
            let dir = match open_attached(root, folder, &mut |_| {}) {
                Ok(Some((dir, _))) => dir,
                Ok(None) => return false,
                Err(_) => return true,
            };
            names.retain(|name| {
                if missing == 0 {
                    return true;
                }
                match stat_at(dir.as_fd(), name) {
                    Ok(stat) => {
                        if let Some(entry) = stat.entry()
                            && let Some(key) = keys_of(&entry).find(|key| wanted.contains_key(key))
                            && let Some(found @ None) = wanted.get_mut(&key)
                        {
                            *found = Some(folder.join(name));
                            missing -= 1;
                        }
                        true
                    }
                    Err(err) => !is_gone(&err),
                }
            });
            !names.is_empty()
        });
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn locate(&self, root: BorrowedFd<'_>, old: &Entry) -> Option<PathBuf> {
        let rel = path_of(root, old.id.ino).ok()??;
        let rel = rel.as_path();
        let left_out = rel.ancestors().any(|path| {
            (path.parent().zip(path.file_name())).is_some_and(|(folder, name)| {
                (self.rules.ignored.get(folder)).is_some_and(|names| names.contains(name))
            })
        });
        if !left_out {
            return None;
        }
        let (dir, _) = open_attached(root, rel.parent()?, &mut |_| {}).ok()??;
        let entry = stat_at(dir.as_fd(), rel.file_name()?).ok()?.entry()?;
        (entry == *old).then(|| rel.to_path_buf())
    }

    #[cfg(not(target_os = "macos"))]
    pub(crate) fn locate(&self, _: BorrowedFd<'_>, _: &Entry) -> Option<PathBuf> {
        None
    }

    pub(crate) fn search(
        &mut self,
        root: BorrowedFd<'_>,
        wanted: &mut Wanted,
        fresh: &HashSet<Key>,
        bounded: bool,
        cut: &mut dyn FnMut(Cut<'_>),
    ) -> HashSet<Key> {
        let Rules {
            ignored,
            budget,
            search,
            ..
        } = &mut *self.rules;
        let Search {
            walks,
            walk: walking,
            pending,
            decided,
            coverage,
            ..
        } = search;
        let next = *walks + 1;
        for (key, found) in wanted.iter_mut() {
            match decided.get(key) {
                Some(Decided::Found(path)) if holds(root, path, *key) => {
                    *found = Some(path.clone())
                }
                Some(Decided::Missed | Decided::Unproven) if !fresh.contains(key) => {}
                _ => {
                    decided.remove(key);
                    let needs = pending.entry(*key).or_insert(next);
                    if fresh.contains(key) {
                        *needs = next;
                    }
                }
            }
        }
        let mut left = if bounded { *budget } else { usize::MAX };
        let mut looked = false;
        let mut checked = false;
        let mut ancestors = Ancestors::default();
        'walk: while (left > 0 || !looked || walking.as_ref().is_some_and(Walk::complete))
            && !pending.is_empty()
        {
            let walk = walking.get_or_insert_with(|| {
                *walks += 1;
                Walk::new(ignored, 0)
            });
            let replaced = 'step: {
                if !checked {
                    checked = true;
                    if walk.folders.is_empty() {
                        walk.base = None;
                    }
                    for at in 0..walk.folders.len() {
                        let Some(above) = walk.above(at) else {
                            continue;
                        };
                        let folder = &walk.folders[at];
                        left = left.saturating_sub(1);
                        cut(Cut::Confirming(&folder.dir));
                        let still = (folder.dir.file_name())
                            .and_then(|name| stat_at(above, name).ok())
                            .and_then(|stat| stat.entry())
                            .is_some_and(|entry| entry.id.same(&folder.id));
                        if !still {
                            let fired = walk.restarts < RESTARTS && gone(root, folder);
                            let mut cost = 0;
                            let fresh = fired
                                && (walk.rounds < CHECKS || walk.quiet(root, at, &mut cost, left));
                            left = left.saturating_sub(cost);
                            if !fresh {
                                break 'step Replaced::Folder(at, None);
                            }
                            let dir = walk.folders[at].dir.clone();
                            walk.restarts += 1;
                            walk.changed = true;
                            walk.folders.truncate(at);
                            walk.reach = None;
                            walk.passed.retain(|path, _| !path.starts_with(&dir));
                            match (
                                walk.folders.last_mut(),
                                walk.listings.last_mut(),
                                dir.file_name(),
                            ) {
                                (Some(parent), _, _) => parent.next -= 1,
                                (None, Some(listing), Some(name)) => {
                                    listing.names.push(name.to_os_string())
                                }
                                _ => {}
                            }
                            break;
                        }
                    }
                    continue 'walk;
                }
                if walk.complete() {
                    walk.finish();
                    continue 'walk;
                }
                if let Some(top) = walk.folders.len().checked_sub(1)
                    && walk.folders[top].fd.is_none()
                {
                    left = left.saturating_sub(1);
                    looked = true;
                    let (at, through) = walk.reach.take().unwrap_or((0, None));
                    let (dir, expected) = (walk.folders[at].dir.clone(), walk.folders[at].id);
                    cut(Cut::OpeningDir(&dir));
                    let opened = (through.as_ref().map(|fd| fd.as_fd()))
                        .or_else(|| walk.above(at))
                        .zip(dir.file_name())
                        .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
                        .and_then(|(above, name)| open_dir_at(above, name))
                        .and_then(|fd| Ok((stat_fd(fd.as_fd())?.id, fd)));
                    match opened {
                        Ok((id, fd)) if id.same(&expected) => {
                            if at + OPEN_FOLDERS <= top {
                                walk.reach = Some((at + 1, Some(fd)));
                            } else {
                                walk.folders[at].fd = Some(fd);
                                walk.reach = (at < top).then_some((at + 1, None));
                            }
                        }
                        Err(err) if !is_gone(&err) => {
                            walk.unopened.insert(dir);
                            walk.folders.truncate(at);
                        }
                        _ => break 'step Replaced::Folder(at, through),
                    }
                    continue 'walk;
                }
                let name = match walk.folders.last_mut() {
                    Some(folder) => {
                        let Some(name) = folder.names.get(folder.next).cloned() else {
                            walk.finish();
                            continue 'walk;
                        };
                        folder.next += 1;
                        name
                    }
                    None => {
                        let Some(listing) = walk.listings.last_mut() else {
                            let Some((dir, chain)) = walk.bases.pop() else {
                                if let Some(dir) = walk.check.pop() {
                                    looked = true;
                                    left = left.saturating_sub(walk.recheck(
                                        root,
                                        dir,
                                        &mut ancestors,
                                        cut,
                                    ));
                                    continue 'walk;
                                }
                                let mut opens = 0;
                                if let Some(stale) = ancestors.pop_to(root, 0, &mut opens) {
                                    walk.changed = true;
                                    walk.owed.insert(stale);
                                }
                                left = left.saturating_sub(opens);
                                if walk.rounds == 0 || (walk.changed && walk.rounds < CHECKS) {
                                    walk.rounds += 1;
                                    walk.changed = false;
                                    walk.owed.clear();
                                    walk.check = walk.passed.keys().rev().cloned().collect();
                                    continue 'walk;
                                }
                                *coverage = Coverage {
                                    ended_early: false,
                                    unopened: std::mem::take(&mut walk.unopened),
                                    replaced: (walk.replaced.iter())
                                        .chain(&walk.owed)
                                        .cloned()
                                        .collect(),
                                };
                                *walking = None;
                                let everywhere = coverage.everywhere();
                                let done = *walks;
                                pending.retain(|key, needs| {
                                    let open = *needs > done;
                                    if !open {
                                        let missed = if everywhere {
                                            Decided::Missed
                                        } else {
                                            Decided::Unproven
                                        };
                                        decided.insert(*key, missed);
                                    }
                                    open
                                });
                                continue 'walk;
                            };
                            left = left.saturating_sub(dir.components().count());
                            looked = true;
                            match open_attached(root, &dir, cut) {
                                Ok(Some((_, now))) if same_chain(&chain, &now) => {}
                                Err(_) => {
                                    walk.unopened.insert(dir);
                                }
                                _ => break 'step Replaced::Path(dir),
                            }
                            continue 'walk;
                        };
                        let Some(name) = listing.names.pop() else {
                            walk.listings.pop();
                            continue 'walk;
                        };
                        let dir = listing.dir.clone();
                        let expected = listing.chain.as_deref();
                        if expected.is_some() || walk.base.as_ref().is_none_or(|(at, _)| *at != dir)
                        {
                            left = left.saturating_sub(dir.components().count());
                            let opened = if expected.is_some() {
                                cut(Cut::Confirming(&dir));
                                open_attached(root, &dir, &mut |_| {})
                            } else {
                                open_attached(root, &dir, cut)
                            };
                            match opened {
                                Ok(Some((fd, chain)))
                                    if expected.is_none_or(|before| same_chain(before, &chain)) =>
                                {
                                    if !chain.is_empty()
                                        && !walk.bases.iter().any(|(at, _)| *at == dir)
                                    {
                                        walk.bases.push((dir.clone(), chain));
                                    }
                                    walk.base = Some((dir, fd));
                                }
                                Ok(_) if expected.is_some() => {
                                    walk.listings.pop();
                                    break 'step Replaced::Path(dir);
                                }
                                gone => {
                                    if gone.is_err() {
                                        walk.unopened.insert(dir);
                                    }
                                    walk.listings.pop();
                                    continue 'walk;
                                }
                            }
                        }
                        name
                    }
                };
                left = left.saturating_sub(1);
                looked = true;
                let (dir, above) = match (walk.folders.last(), &walk.base) {
                    (Some(folder), _) => (&folder.dir, folder.fd.as_ref()),
                    (None, Some((dir, fd))) => (dir, Some(fd)),
                    (None, None) => continue 'walk,
                };
                let Some(above) = above.map(|fd| fd.as_fd()) else {
                    continue 'walk;
                };
                let path = dir.join(&name);
                cut(Cut::Searching(&path));
                let entry = match stat_at(above, &name) {
                    Ok(stat) => {
                        let entry = stat.entry();
                        if walk.folders.is_empty()
                            && entry.is_none_or(|entry| entry.kind != EntryKind::Dir)
                        {
                            walk.passed.remove(&path);
                        }
                        entry
                    }
                    Err(err) if is_gone(&err) => {
                        walk.passed.entry(path.clone()).or_insert(Seen {
                            id: None,
                            stamp: (0, 0),
                            racy: false,
                            listing: 0,
                            needs_walk: true,
                            done: false,
                        });
                        None
                    }
                    Err(_) => {
                        walk.unopened.insert(path.clone());
                        None
                    }
                };
                let Some(entry) = entry else { continue 'walk };
                let keys: Vec<Key> = keys_of(&entry)
                    .filter(|key| pending.contains_key(key))
                    .collect();
                if let Some(&key) = keys.first() {
                    left = left.saturating_sub(path.components().count());
                    if !holds(root, &path, key) {
                        break 'step Replaced::Path(path);
                    }
                }
                for key in keys {
                    pending.remove(&key);
                    if let Some(found) = wanted.get_mut(&key) {
                        *found = Some(path.clone());
                    }
                    decided.insert(key, Decided::Found(path.clone()));
                }
                if entry.kind != EntryKind::Dir {
                    continue 'walk;
                }
                if (walk.passed.get(&path))
                    .is_some_and(|seen| seen.done && !seen.needs_walk && seen.is(&entry.id))
                {
                    continue 'walk;
                }
                left = left.saturating_sub(1);
                cut(Cut::OpeningDir(&path));
                let opened =
                    open_dir_at(above, &name).and_then(|sub| Ok((stat_fd(sub.as_fd())?.id, sub)));
                let sub = match opened {
                    Ok((id, sub)) if id.same(&entry.id) => sub,
                    Err(err) if !is_gone(&err) => {
                        walk.unopened.insert(path);
                        continue 'walk;
                    }
                    _ => break 'step Replaced::Path(path),
                };
                let Some((seen, names)) = Seen::of(sub.as_fd(), &path, &mut walk.unopened) else {
                    continue 'walk;
                };
                walk.passed.insert(path.clone(), seen);
                walk.push(Folder {
                    dir: path,
                    id: entry.id,
                    names,
                    next: 0,
                    fd: Some(sub),
                });
                continue 'walk;
            };
            if walk.restarts < RESTARTS {
                let restarts = walk.restarts + 1;
                *walks += 1;
                *walking = Some(Walk::new(ignored, restarts));
                ancestors = Ancestors::default();
                for needs in pending.values_mut() {
                    *needs = *walks;
                }
                continue 'walk;
            }
            walk.leave(replaced);
        }
        if let Some(walk) = walking.as_mut()
            && let Some(stale) = ancestors.pop_to(root, 0, &mut 0)
        {
            walk.changed = true;
            walk.owed.insert(stale);
        }
        if pending.is_empty() {
            *walking = None;
        }
        (wanted.keys())
            .filter(|key| pending.contains_key(key))
            .copied()
            .collect()
    }

    pub(crate) fn unproven(&self, key: Key) -> bool {
        matches!(self.rules.search.decided.get(&key), Some(Decided::Unproven))
    }

    pub(crate) fn held_in(&self, scopes: &[PathBuf]) -> BTreeMap<PathBuf, Entry> {
        (self.rules.search.held.iter())
            .filter(|(path, _)| scopes.iter().any(|scope| path.starts_with(scope)))
            .map(|(path, entry)| (path.clone(), *entry))
            .collect()
    }

    pub(crate) fn take_held(
        &mut self,
        scopes: &[PathBuf],
        stays: &[PathBuf],
    ) -> BTreeMap<PathBuf, Entry> {
        let held = &mut self.rules.search.held;
        let (taken, kept) = std::mem::take(held).into_iter().partition(|(path, _)| {
            scopes.iter().any(|scope| path.starts_with(scope))
                && !stays.iter().any(|kept| path.starts_with(kept))
        });
        *held = kept;
        taken
    }

    pub(crate) fn seek(&mut self, keys: HashSet<Key>) {
        let search = &mut self.rules.search;
        search.sought = keys;
        search.prune();
    }

    pub(crate) fn hold(&mut self, held: BTreeMap<PathBuf, Entry>) {
        let search = &mut self.rules.search;
        search.held.extend(held);
        search.prune();
    }

    pub(crate) fn held(&self) -> impl Iterator<Item = &PathBuf> {
        self.rules.search.held.keys()
    }

    pub(crate) fn postpone(
        &mut self,
        scopes: &[PathBuf],
        paths: impl IntoIterator<Item = PathBuf>,
    ) {
        let postponed = &mut self.rules.search.postponed;
        for scope in scopes {
            let inside: Vec<PathBuf> = (postponed.range(scope.clone()..))
                .take_while(|path| path.starts_with(scope))
                .cloned()
                .collect();
            for path in inside {
                postponed.remove(&path);
            }
        }
        postponed.extend(paths);
    }

    pub(crate) fn postponed(&self) -> impl Iterator<Item = &PathBuf> {
        self.rules.search.postponed.iter()
    }

    pub(crate) fn cover(&mut self, scopes: &[PathBuf], unread: &[PathBuf]) -> Seeded {
        let search = &mut self.rules.search;
        let seeded = Seeded {
            scanned: search.scanned.clone(),
            reseeded: search.reseeded.clone(),
        };
        let under =
            |path: &Path, paths: &[PathBuf]| paths.iter().any(|above| path.starts_with(above));
        let still: Vec<PathBuf> = (unread.iter())
            .filter(|path| seeded.unscanned(path))
            .chain(seeded.reseeded.iter().filter(|path| under(path, unread)))
            .cloned()
            .collect();
        search.scanned.extend(scopes.iter().cloned());
        search.scanned = crate::outermost(std::mem::take(&mut search.scanned));
        search.reseeded.retain(|path| !under(path, scopes));
        search.reseeded.extend(still);
        search.reseeded = crate::outermost(std::mem::take(&mut search.reseeded));
        seeded
    }

    pub(crate) fn finish(
        self,
        is_folder: impl Fn(&Path) -> bool,
        is_left_out_folder: impl Fn(&Path) -> bool,
    ) -> Finished {
        let Self {
            rules,
            root,
            mut before,
            mut tracked,
            read,
            ..
        } = self;
        let held = |dir: &Path| dir.as_os_str().is_empty() || is_folder(dir);
        let marked = |tree: &Path| {
            is_left_out_folder(tree)
                && std::fs::symlink_metadata(root.join(tree).join(".git")).is_ok()
        };
        let gone: Vec<PathBuf> = (rules.indexes.keys().chain(rules.tracked.keys()))
            .filter(|tree| !held(tree) && !marked(tree))
            .cloned()
            .collect();
        let forgotten: Vec<_> = gone
            .iter()
            .flat_map(|tree| rules.forget_index(tree))
            .collect();
        let unsettled = (forgotten.iter())
            .flat_map(|(tree, before)| rules.rescope(tree, before, &is_folder))
            .collect();
        for (tree, before) in forgotten {
            tracked.entry(tree).or_insert(before);
        }
        let tracked = (tracked.into_iter())
            .filter_map(|(tree, before)| {
                let now = rules.tracked.get(&tree).cloned().unwrap_or_default();
                let respelled = now.is_empty()
                    && (rules.tracked.keys()).any(|live| rules.key(live) == rules.key(&tree));
                (now != before && !respelled).then_some((tree, now))
            })
            .collect();
        let stale: Vec<(PathBuf, Source)> = (rules.files.iter())
            .filter(|(_, (dir, _))| !held(dir))
            .map(|((_, source), (dir, _))| (dir.clone(), *source))
            .collect();
        for (dir, source) in stale {
            let key = (rules.key(&dir), source);
            let dropped = rules.set(&dir, source, None);
            before.entry(key).or_insert(dropped);
        }
        rules.ignored.retain(|dir, _| held(dir));
        let changed = (before.iter())
            .filter_map(|(key, before)| match (rules.files.get(key), before) {
                (Some((_, now)), Some((_, before))) if now == before => None,
                (Some((dir, now)), _) => Some((dir.clone(), key.1, Some(now.clone()))),
                (None, Some((dir, _))) => Some((dir.clone(), key.1, None)),
                (None, None) => None,
            })
            .collect();
        Finished {
            rules: changed,
            read: read.into_iter().filter(|dir| held(dir)).collect(),
            tracked,
            unsettled,
        }
    }
}

pub(crate) type RulesFile = (PathBuf, Source, Option<Vec<u8>>);

pub(crate) struct Finished {
    pub(crate) rules: Vec<RulesFile>,
    pub(crate) read: Vec<PathBuf>,
    pub(crate) tracked: Vec<(PathBuf, Vec<String>)>,
    /// The scopes whose answer changed after the traversal, which it judged by the lists before.
    pub(crate) unsettled: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RulesRead {
    Same,
    Changed,
    Unknown,
}

fn read_rules(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<Option<Vec<u8>>> {
    let regular = |stat: &Stat| stat.file_type == FileType::RegularFile;
    match stat_at(dir, name) {
        Ok(stat) if regular(&stat) => {}
        Ok(_) => return Ok(None),
        Err(err) if is_gone(&err) => return Ok(None),
        Err(err) => return Err(err),
    }
    // Without NONBLOCK, opening a FIFO that took the file's place would wait
    // for a writer.
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let file = match rustix::fs::openat(dir, name, flags, Mode::empty()) {
        Ok(file) => file,
        Err(err) if is_gone(&err.into()) => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let stat = stat_fd(file.as_fd())?;
    if !regular(&stat) {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    if stat.size <= MAX_RULES {
        std::fs::File::from(file)
            .take(MAX_RULES + 1)
            .read_to_end(&mut bytes)?;
    }
    if stat.size > MAX_RULES || bytes.len() as u64 > MAX_RULES {
        return Err(io::ErrorKind::FileTooLarge.into());
    }
    Ok(Some(bytes))
}

#[cfg(target_os = "macos")]
pub(crate) fn path_of(root: BorrowedFd<'_>, ino: u128) -> io::Result<Option<PathBuf>> {
    unsafe extern "C" {
        fn fsgetpath(
            buf: *mut std::ffi::c_char,
            size: usize,
            fsid: *const rustix::fs::Fsid,
            obj_id: u64,
        ) -> isize;
    }
    let Ok(ino) = u64::try_from(ino) else {
        return Ok(None);
    };
    let fsid = rustix::fs::fstatfs(root)?.f_fsid;
    let base = rustix::fs::getpath(root)?;
    let mut buf = vec![0u8; 4096];
    let len = unsafe { fsgetpath(buf.as_mut_ptr().cast(), buf.len(), &fsid, ino) };
    let Ok(len) = usize::try_from(len) else {
        let err = io::Error::last_os_error();
        return match err.kind() {
            io::ErrorKind::NotFound => Ok(None),
            _ => Err(err),
        };
    };
    let found = buf
        .get(..len)
        .and_then(|bytes| std::ffi::CStr::from_bytes_until_nul(bytes).ok());
    Ok(found.and_then(|found| {
        Path::new(OsStr::from_bytes(found.to_bytes()))
            .strip_prefix(OsStr::from_bytes(base.as_bytes()))
            .ok()
            .map(Path::to_path_buf)
    }))
}

fn holds(root: BorrowedFd<'_>, path: &Path, key: Key) -> bool {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return false;
    };
    let Ok(Some((dir, _))) = open_attached(root, parent, &mut |_| {}) else {
        return false;
    };
    (stat_at(dir.as_fd(), name).ok())
        .and_then(|stat| stat.entry())
        .is_some_and(|entry| entry.id.same(&key.0) && entry.kind == key.1)
}

#[cfg(test)]
mod rechecks {
    use super::*;
    use std::fs;

    struct IgnoredFolder;

    impl Ignore for IgnoredFolder {
        fn set_rules(&mut self, _: &Path, _: Source, _: Option<&[u8]>) {}

        fn status(&self, path: &Path, _: EntryKind) -> Ignored {
            if path.starts_with("ig") {
                Ignored::Rule
            } else {
                Ignored::No
            }
        }

        fn key(&self, name: &OsStr) -> OsString {
            name.to_os_string()
        }
    }

    fn replace_before_replay(replace: &str, restarts: usize) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("ig/a")).unwrap();
        fs::write(root.join("ig/a/old"), "old").unwrap();
        fs::write(root.join("want"), "want").unwrap();
        let fd = crate::open_root(root).unwrap();
        let id = stat_at(fd.as_fd(), OsStr::new("want")).unwrap().id;
        let key = (id, EntryKind::File);
        let mut rules = Rules::new(Box::new(IgnoredFolder)).with_search_budget(1);
        rules
            .ignored
            .insert(PathBuf::new(), BTreeSet::from(["ig".into()]));
        let mut walk = Walk::new(&rules.ignored, restarts);
        walk.listings.clear();
        walk.rounds = CHECKS;
        walk.check.push("ig/a".into());
        let (child, _) = open_attached(fd.as_fd(), Path::new("ig/a"), &mut |_| {})
            .unwrap()
            .unwrap();
        let (seen, _) = Seen::of(child.as_fd(), Path::new("ig/a"), &mut walk.unopened).unwrap();
        walk.passed.insert("ig/a".into(), seen);
        rules.search.walk = Some(walk);
        rules.search.walks = 1;
        rules.search.pending.insert(key, 1);
        fs::write(root.join("ig/a/new"), "new").unwrap();
        let mut wanted = HashMap::from([(key, None)]);
        let mut search = Pending::new(&mut rules, root);
        assert_eq!(
            search.search(fd.as_fd(), &mut wanted, &HashSet::new(), true, &mut |_| {}),
            HashSet::from([key])
        );
        if !replace.is_empty() {
            fs::rename(root.join(replace), root.join("parked")).unwrap();
            fs::create_dir_all(root.join("ig/a")).unwrap();
        }
        fs::rename(root.join("want"), root.join("ig/a/want")).unwrap();
        assert!(
            search
                .search(fd.as_fd(), &mut wanted, &HashSet::new(), false, &mut |_| {})
                .is_empty()
        );
        if replace == "ig" && restarts == RESTARTS {
            assert!(search.unproven(key));
            assert_eq!(wanted[&key], None);
        } else {
            assert_eq!(
                wanted[&key],
                Some("ig/a/want".into()),
                "replace {replace:?}"
            );
        }
    }

    #[test]
    fn a_search_uses_the_listing_of_the_folder_it_opens_after_a_recheck() {
        replace_before_replay("ig/a", RESTARTS);
        replace_before_replay("", RESTARTS);
    }

    #[test]
    fn a_search_checks_the_identity_of_the_parent_before_replaying_a_folder() {
        replace_before_replay("ig", RESTARTS);
        replace_before_replay("ig", 0);
    }

    #[test]
    fn a_search_walks_a_folder_it_left_unfinished_when_it_meets_it_again() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("ig/p/c")).unwrap();
        fs::write(root.join("ig/p/c/a"), "a").unwrap();
        fs::write(root.join("ig/p/c/b"), "b").unwrap();
        fs::write(root.join("want"), "want").unwrap();
        let fd = crate::open_root(root).unwrap();
        let id = stat_at(fd.as_fd(), OsStr::new("want")).unwrap().id;
        let key = (id, EntryKind::File);
        fs::rename(root.join("want"), root.join("ig/p/c/zz")).unwrap();
        let mut rules = Rules::new(Box::new(IgnoredFolder)).with_search_budget(1);
        rules
            .ignored
            .insert(PathBuf::new(), BTreeSet::from(["ig".into()]));
        rules.search.walk = Some(Walk::new(&rules.ignored, RESTARTS));
        rules.search.walks = 1;
        rules.search.pending.insert(key, 1);
        let mut wanted = HashMap::from([(key, None)]);
        let mut search = Pending::new(&mut rules, root);
        let inside = |search: &Pending<'_>| {
            let top = search.rules.search.walk.as_ref().unwrap().folders.last();
            top.is_some_and(|top| top.dir == Path::new("ig/p/c") && top.next == 1)
        };
        while !inside(&search) {
            search.search(fd.as_fd(), &mut wanted, &HashSet::new(), true, &mut |_| {});
        }
        fs::create_dir(root.join("ig/new")).unwrap();
        fs::rename(root.join("ig/p/c"), root.join("ig/new/c")).unwrap();
        fs::rename(root.join("ig/p"), root.join("ig/old")).unwrap();
        fs::rename(root.join("ig/new"), root.join("ig/p")).unwrap();
        search.search(fd.as_fd(), &mut wanted, &HashSet::new(), false, &mut |_| {});
        assert_eq!(wanted[&key], Some("ig/p/c/zz".into()));
    }

    #[test]
    fn a_search_lists_a_recreated_folder_in_place_only_when_nothing_it_held_can_be_elsewhere() {
        for (moved, found) in [
            ("ig/a/moved", Some("ig/a/moved/zz")),
            ("ig/a/b/c/moved", Some("ig/a/b/c/moved/zz")),
            ("ig/z/moved", Some("ig/z/moved/zz")),
            ("outside", None),
            ("deleted", None),
            ("drained", Some("ig/a/zz")),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let away = tempfile::tempdir().unwrap();
            let root = temp.path();
            for path in ["ig/a/b/c/keep", "ig/p/x1", "ig/p/x2", "ig/z/keep", "want"] {
                fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
                fs::write(root.join(path), "x").unwrap();
            }
            let fd = crate::open_root(root).unwrap();
            let id = stat_at(fd.as_fd(), OsStr::new("want")).unwrap().id;
            let key = (id, EntryKind::File);
            let mut rules = Rules::new(Box::new(IgnoredFolder)).with_search_budget(1);
            rules
                .ignored
                .insert(PathBuf::new(), BTreeSet::from(["ig".into()]));
            rules.search.walk = Some(Walk::new(&rules.ignored, RESTARTS - 1));
            rules.search.walks = 1;
            rules.search.pending.insert(key, 1);
            let mut wanted = HashMap::from([(key, None)]);
            let mut search = Pending::new(&mut rules, root);
            while search.rules.search.walk.as_ref().unwrap().rounds == 0 {
                search.search(fd.as_fd(), &mut wanted, &HashSet::new(), true, &mut |_| {});
            }
            search.rules.search.walk.as_mut().unwrap().rounds = CHECKS;
            fs::rename(root.join("want"), root.join("ig/p/zz")).unwrap();
            let inside = |search: &Pending<'_>| {
                let top = search.rules.search.walk.as_ref().unwrap().folders.last();
                top.is_some_and(|top| top.dir == Path::new("ig/p") && top.next == 1)
            };
            while !inside(&search) {
                search.search(fd.as_fd(), &mut wanted, &HashSet::new(), true, &mut |_| {});
            }
            match moved {
                "deleted" => fs::remove_dir_all(root.join("ig/p")).unwrap(),
                "outside" => fs::rename(root.join("ig/p"), away.path().join("p")).unwrap(),
                "drained" => {
                    fs::rename(root.join("ig/p/zz"), root.join("ig/a/zz")).unwrap();
                    fs::remove_file(root.join("ig/p/x1")).unwrap();
                    fs::remove_file(root.join("ig/p/x2")).unwrap();
                    fs::remove_dir(root.join("ig/p")).unwrap();
                }
                to => fs::rename(root.join("ig/p"), root.join(to)).unwrap(),
            }
            fs::create_dir(root.join("ig/p")).unwrap();
            search.search(fd.as_fd(), &mut wanted, &HashSet::new(), false, &mut |_| {});
            assert_eq!(wanted[&key], found.map(PathBuf::from), "{moved}");
            assert!(!search.unproven(key), "{moved}");
        }
    }
}
