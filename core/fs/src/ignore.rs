// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
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
}

impl Coverage {
    /// Whether the search looked everywhere, so that an entry it did not find is not on the disk.
    pub fn everywhere(&self) -> bool {
        !self.ended_early && self.unopened.is_empty()
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
}

struct Walk {
    listings: Vec<Listing>,
    folders: Vec<Folder>,
    later: Vec<(PathBuf, FileId)>,
    reach: Option<Reach>,
    above: Option<OwnedFd>,
    bases: Vec<(PathBuf, Vec<FileId>)>,
    unopened: BTreeSet<PathBuf>,
}

struct Reach {
    dir: PathBuf,
    expect: Vec<FileId>,
    seen: Vec<FileId>,
    fd: OwnedFd,
    confirm: bool,
}

struct Folder {
    dir: PathBuf,
    id: FileId,
    entries: Dir,
    names: Vec<OsString>,
}

impl Folder {
    fn open(
        dir: PathBuf,
        id: FileId,
        fd: OwnedFd,
        unopened: &mut BTreeSet<PathBuf>,
    ) -> Option<Self> {
        let Ok(mut entries) = Dir::new(fd) else {
            unopened.insert(dir);
            return None;
        };
        let mut names: Vec<OsString> = Vec::new();
        for item in std::iter::from_fn(|| entries.read()) {
            let Ok(item) = item else {
                unopened.insert(dir.clone());
                break;
            };
            names.push(OsStr::from_bytes(item.file_name().to_bytes()).to_os_string());
        }
        names.retain(|name| *name != "." && *name != "..");
        names.sort_unstable_by(|a, b| b.cmp(a));
        Some(Self {
            dir,
            id,
            entries,
            names,
        })
    }
}

const OPEN_FOLDERS: usize = 16;

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
        search.coverage = Coverage {
            ended_early: true,
            unopened: (search.walk.take()).map_or_else(BTreeSet::new, |walk| walk.unopened),
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
        while (left > 0 || !looked) && !pending.is_empty() {
            let walk = walking.get_or_insert_with(|| {
                *walks += 1;
                Walk {
                    listings: (ignored.iter())
                        .map(|(dir, names)| Listing {
                            dir: dir.clone(),
                            names: names.iter().rev().cloned().collect(),
                        })
                        .rev()
                        .collect(),
                    folders: Vec::new(),
                    later: Vec::new(),
                    reach: None,
                    above: None,
                    bases: Vec::new(),
                    unopened: BTreeSet::new(),
                }
            });
            let mut opened: Option<(PathBuf, OwnedFd)> = None;
            let mut moved = false;
            if !checked {
                checked = true;
                let mut parent = walk.above.as_ref().map(|fd| fd.as_fd());
                for folder in &walk.folders {
                    left = left.saturating_sub(1);
                    cut(Cut::Confirming(&folder.dir));
                    let still = (parent.zip(folder.dir.file_name()))
                        .and_then(|(parent, name)| stat_at(parent, name).ok())
                        .and_then(|stat| stat.entry())
                        .is_some_and(|entry| entry.id.same(&folder.id));
                    if !still {
                        moved = true;
                        break;
                    }
                    parent = folder.entries.fd().ok();
                }
            }
            while !moved && (left > 0 || !looked) && !pending.is_empty() {
                let (dir, name) = match walk.folders.last_mut() {
                    Some(Folder { dir, names, .. }) => {
                        let Some(name) = names.pop() else {
                            walk.folders.pop();
                            continue;
                        };
                        (dir.clone(), name)
                    }
                    None => {
                        if walk.reach.is_none() && !walk.later.is_empty() {
                            let Ok(fd) = root.try_clone_to_owned() else {
                                (left, looked) = (0, true);
                                break;
                            };
                            walk.reach = (walk.later.pop()).map(|(dir, id)| Reach {
                                dir,
                                expect: vec![id],
                                seen: Vec::new(),
                                fd,
                                confirm: false,
                            });
                            continue;
                        }
                        if let Some(reach) = &mut walk.reach {
                            left = left.saturating_sub(1);
                            looked = true;
                            let depth = reach.dir.components().count();
                            let below: PathBuf =
                                reach.dir.iter().take(reach.seen.len() + 1).collect();
                            cut(Cut::OpeningDir(&below));
                            let at = (reach.seen.len() + reach.expect.len()).checked_sub(depth);
                            let opened = (below.file_name())
                                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
                                .and_then(|name| open_dir_at(reach.fd.as_fd(), name))
                                .and_then(|next| Ok((stat_fd(next.as_fd())?.id, next)));
                            let (id, next) = match opened {
                                Ok((id, next))
                                    if at.is_none_or(|at| {
                                        reach.expect.get(at).is_some_and(|dir| dir.same(&id))
                                    }) =>
                                {
                                    (id, next)
                                }
                                Err(err) if !is_gone(&err) => {
                                    walk.unopened.insert(below);
                                    walk.reach = None;
                                    continue;
                                }
                                _ => {
                                    moved = true;
                                    break;
                                }
                            };
                            reach.seen.push(id);
                            let before = std::mem::replace(&mut reach.fd, next);
                            if reach.seen.len() < depth {
                                continue;
                            }
                            let Some(Reach {
                                dir,
                                mut seen,
                                fd,
                                confirm,
                                ..
                            }) = walk.reach.take()
                            else {
                                continue;
                            };
                            if confirm {
                                continue;
                            }
                            let Some(folder) = Folder::open(dir, id, fd, &mut walk.unopened) else {
                                continue;
                            };
                            seen.pop();
                            if let Some(parent) = folder.dir.parent()
                                && !walk.bases.iter().any(|(at, _)| at == parent)
                            {
                                walk.bases.push((parent.to_path_buf(), seen));
                            }
                            walk.above = Some(before);
                            walk.folders.push(folder);
                            continue;
                        }
                        let Some(listing) = walk.listings.last_mut() else {
                            if walk.later.is_empty() && !walk.bases.is_empty() {
                                let Ok(fd) = root.try_clone_to_owned() else {
                                    (left, looked) = (0, true);
                                    break;
                                };
                                walk.reach = (walk.bases.pop()).map(|(dir, expect)| Reach {
                                    dir,
                                    expect,
                                    seen: Vec::new(),
                                    fd,
                                    confirm: true,
                                });
                                continue;
                            }
                            break;
                        };
                        let Some(name) = listing.names.pop() else {
                            walk.listings.pop();
                            continue;
                        };
                        (listing.dir.clone(), name)
                    }
                };
                left = left.saturating_sub(1);
                looked = true;
                if walk.folders.is_empty() && opened.as_ref().is_none_or(|(at, _)| *at != dir) {
                    left = left.saturating_sub(dir.components().count());
                    opened = match open_attached(root, &dir, cut) {
                        Ok(Some((fd, chain))) => {
                            if !chain.is_empty() && !walk.bases.iter().any(|(at, _)| *at == dir) {
                                walk.bases.push((dir.clone(), chain));
                            }
                            Some((dir.clone(), fd))
                        }
                        gone => {
                            if gone.is_err() {
                                walk.unopened.insert(dir);
                            }
                            walk.listings.pop();
                            continue;
                        }
                    };
                }
                let parent = match (walk.folders.last(), &opened) {
                    (Some(Folder { entries, .. }), _) => entries.fd().ok(),
                    (None, Some((_, fd))) => Some(fd.as_fd()),
                    (None, None) => None,
                };
                let Some(parent) = parent else { continue };
                let path = dir.join(&name);
                cut(Cut::Searching(&path));
                let entry = match stat_at(parent, &name) {
                    Ok(stat) => stat.entry(),
                    Err(err) => {
                        if !is_gone(&err) {
                            walk.unopened.insert(path.clone());
                        }
                        None
                    }
                };
                let Some(entry) = entry else { continue };
                let keys: Vec<Key> = keys_of(&entry)
                    .filter(|key| pending.contains_key(key))
                    .collect();
                if let Some(&key) = keys.first() {
                    left = left.saturating_sub(path.components().count());
                    if !holds(root, &path, key) {
                        moved = true;
                        break;
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
                    continue;
                }
                if walk.folders.len() >= OPEN_FOLDERS {
                    walk.later.push((path, entry.id));
                    continue;
                }
                left = left.saturating_sub(1);
                cut(Cut::OpeningDir(&path));
                let sub = match open_dir_at(parent, &name) {
                    Ok(sub) => sub,
                    Err(err) => {
                        if !is_gone(&err) {
                            walk.unopened.insert(path);
                        }
                        continue;
                    }
                };
                let Some(folder) = Folder::open(path, entry.id, sub, &mut walk.unopened) else {
                    continue;
                };
                if walk.folders.is_empty() {
                    walk.above = opened.take().map(|(_, fd)| fd);
                }
                walk.folders.push(folder);
            }
            if moved {
                *walking = None;
                let again = *walks + 1;
                for needs in pending.values_mut() {
                    *needs = again;
                }
                continue;
            }
            if walk.listings.is_empty()
                && walk.folders.is_empty()
                && walk.later.is_empty()
                && walk.reach.is_none()
                && walk.bases.is_empty()
            {
                *coverage = Coverage {
                    ended_early: false,
                    unopened: std::mem::take(&mut walk.unopened),
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
            }
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
