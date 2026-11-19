// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use std::ffi::{OsStr, OsString};

use std::fs;

use std::os::fd::AsFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

use std::path::{Path, PathBuf};

use std::time::{Duration, Instant};

use koidrive_fs::{
    Change, EntryKind, FileId, Ignore, Ignored, Known, NativeWatcher, Poll, RescanWatcher, Rules,
    Snapshot, Source, Watcher,
};

use koidrive_ignore::{Kind, LocalRules, Matcher, NameKey, Reason, RuleFile, Status};

use EntryKind::{Dir, File};

const WAIT: Duration = Duration::from_secs(10);

const KOIIGNORE: &str = Source::Koiignore.file_name();

struct Folded;

impl NameKey for Folded {
    fn key(name: &str) -> String {
        koidrive_engine::names::name_key(name)
    }
}

struct Matched(Matcher<Folded>);

fn names(path: &Path) -> Vec<String> {
    path.iter()
        .map(|name| name.to_string_lossy().into_owned())
        .collect()
}

impl Ignore for Matched {
    fn set_rules(&mut self, dir: &Path, source: Source, rules: Option<&[u8]>) {
        self.0
            .set_rules(&names(dir), source, rules.map(RuleFile::parse), []);
    }

    fn status(&self, path: &Path, kind: EntryKind) -> Ignored {
        let kind = match kind {
            File => Kind::File,
            Dir => Kind::Dir,
        };
        match self.0.status(&names(path), kind) {
            Status::Included => Ignored::No,
            Status::Excluded(Reason::BuiltIn) => Ignored::BuiltIn,
            Status::Excluded(_) => Ignored::Rule,
        }
    }

    fn key(&self, name: &OsStr) -> OsString {
        Folded::key(&name.to_string_lossy()).into()
    }
}

fn rules() -> Rules {
    Rules::new(Box::new(Matched(Matcher::new(LocalRules::default()))))
}

fn write(root: &Path, path: &str, body: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn known(snapshot: &Snapshot) -> Vec<&str> {
    snapshot
        .entries
        .keys()
        .map(|path| path.to_str().unwrap())
        .collect()
}

fn created(path: &str, kind: EntryKind) -> Change {
    Change::Created {
        path: path.into(),
        kind,
    }
}

fn removed(path: &str, kind: EntryKind) -> Change {
    Change::Removed {
        path: path.into(),
        kind,
    }
}

type RulesFile = (PathBuf, Source, Option<Vec<u8>>);

fn rule(dir: &str, rules: Option<&str>) -> RulesFile {
    (
        dir.into(),
        Source::Koiignore,
        rules.map(|rules| rules.as_bytes().to_vec()),
    )
}

fn git_rule(dir: &str, rules: Option<&str>) -> RulesFile {
    (
        dir.into(),
        Source::Gitignore,
        rules.map(|rules| rules.as_bytes().to_vec()),
    )
}

fn whole() -> [PathBuf; 1] {
    [PathBuf::new()]
}

fn tree_without_rules() -> (tempfile::TempDir, Snapshot, Rules) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "c.tmp", "c");
    write(root, "sub/a.tmp", "a");
    write(root, "sub/b.txt", "b");
    write(root, "sub/cache/x/y.txt", "y");
    let mut rules = rules();
    let snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    assert_eq!(
        known(&snapshot),
        [
            "c.tmp",
            "sub",
            "sub/a.tmp",
            "sub/b.txt",
            "sub/cache",
            "sub/cache/x",
            "sub/cache/x/y.txt"
        ]
    );
    (dir, snapshot, rules)
}

fn tree_with_an_ignored_folder() -> (tempfile::TempDir, Snapshot, Rules) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, KOIIGNORE, "ig/\n");
    write(root, "ig/deep/old.txt", "o");
    write(root, "f.txt", "f");
    write(root, "d/g.txt", "g");
    let mut rules = rules();
    let snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    assert_eq!(known(&snapshot), [KOIIGNORE, "d", "d/g.txt", "f.txt"]);
    (dir, snapshot, rules)
}

fn large_ignored_tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, KOIIGNORE, "ig/\n");
    for n in 0..30 {
        for m in 0..10 {
            write(root, &format!("ig/{n:02}/{m}.txt"), "x");
        }
    }
    fs::create_dir_all(root.join("ig/zz/deep")).unwrap();
    dir
}

#[derive(Default)]
struct Random(u64);

impl Random {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }

    fn pick<T: Clone>(&mut self, items: &[T]) -> Option<T> {
        (!items.is_empty()).then(|| items[self.below(items.len())].clone())
    }
}

fn listed(root: &Path, under: &Path, into: &mut Vec<(PathBuf, bool)>) {
    for entry in fs::read_dir(root.join(under)).unwrap() {
        let entry = entry.unwrap();
        let path = under.join(entry.file_name());
        let dir = entry.file_type().unwrap().is_dir();
        into.push((path.clone(), dir));
        if dir {
            listed(root, &path, into);
        }
    }
}

fn on_disk(root: &Path) -> Vec<FileId> {
    let mut all = Vec::new();
    listed(root, Path::new(""), &mut all);
    (all.iter())
        .map(|(path, _)| {
            let opened = fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(root.join(path))
                .unwrap();
            koidrive_fs::id_of(opened.as_fd()).unwrap()
        })
        .collect()
}

#[derive(Default)]
struct Changer {
    random: Random,
    fresh: usize,
    vacated: Vec<PathBuf>,
    out: BTreeSet<u128>,
    bounced: BTreeSet<u128>,
}

impl Changer {
    fn change(&mut self, root: &Path) {
        let mut all = Vec::new();
        listed(root, Path::new(""), &mut all);
        let included: Vec<(PathBuf, bool)> = (all.iter())
            .filter(|(path, _)| !path.starts_with("ig") && path != Path::new(KOIIGNORE))
            .cloned()
            .collect();
        let folders = |ignored: bool| -> Vec<PathBuf> {
            (all.iter())
                .filter(|(path, dir)| *dir && path.starts_with("ig") == ignored)
                .map(|(path, _)| path.clone())
                .chain((!ignored).then(PathBuf::new))
                .collect()
        };
        self.fresh += 1;
        let name = format!("n{}", self.fresh);
        let moved = |from: &Path, to: &Path| {
            if !to.starts_with(from) && !root.join(to).exists() {
                fs::rename(root.join(from), root.join(to)).ok();
            }
        };
        let ids = |path: &Path| -> Vec<u128> {
            let mut all = vec![(path.to_path_buf(), root.join(path).is_dir())];
            if all[0].1 {
                listed(root, path, &mut all);
            }
            (all.iter())
                .filter_map(|(path, _)| fs::symlink_metadata(root.join(path)).ok())
                .map(|meta| u128::from(meta.ino()))
                .collect()
        };
        let free = (self.vacated.iter())
            .filter(|path| {
                !root.join(path).exists()
                    && (path.parent()).is_some_and(|parent| root.join(parent).is_dir())
            })
            .cloned()
            .collect::<Vec<_>>();
        match self.random.below(8) {
            0 => {
                if let Some((path, false)) = self.random.pick(&included) {
                    fs::remove_file(root.join(path)).unwrap();
                }
            }
            1 => {
                if let Some((path, true)) = self.random.pick(&included) {
                    fs::remove_dir_all(root.join(path)).unwrap();
                }
            }
            2 => {
                if let (Some((from, _)), Some(into)) = (
                    self.random.pick(&included),
                    self.random.pick(&folders(false)),
                ) {
                    moved(&from, &into.join(&name));
                }
            }
            3 => {
                if let (Some((from, _)), Some(into)) = (
                    self.random.pick(&included),
                    self.random.pick(&folders(true)),
                ) {
                    let inside = ids(&from);
                    moved(&from, &into.join(&name));
                    self.vacated.push(from);
                    (self.bounced).extend(inside.into_iter().filter(|id| self.out.contains(id)));
                }
            }
            4 => {
                if let Some(into) = self.random.pick(&folders(false)) {
                    write(root, &into.join(&name).to_string_lossy(), "n");
                }
            }
            5 => {
                if let Some(path) = self.random.pick(&free) {
                    write(root, &path.to_string_lossy(), "new");
                }
            }
            6 => {
                if let (Some(to), Some((from, false))) =
                    (self.random.pick(&free), self.random.pick(&included))
                {
                    moved(&from, &to);
                }
            }
            _ => {
                let ignored: Vec<PathBuf> = (all.iter())
                    .filter(|(path, _)| path.starts_with("ig") && path.components().count() > 2)
                    .map(|(path, _)| path.clone())
                    .collect();
                if let (Some(from), Some(into)) = (
                    self.random.pick(&ignored),
                    self.random.pick(&folders(false)),
                ) {
                    self.out.extend(ids(&from));
                    moved(&from, &into.join(&name));
                }
            }
        }
    }
}

fn identities(snapshot: &Snapshot) -> BTreeMap<PathBuf, FileId> {
    (snapshot.entries.iter())
        .map(|(path, entry)| (path.clone(), entry.id))
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Node {
    id: FileId,
    born: (usize, PathBuf),
}

type Gone = ((usize, PathBuf), Option<PathBuf>);

#[derive(Default)]
struct Synced {
    run: String,
    step: usize,
    nodes: BTreeMap<PathBuf, Node>,
    gone: Vec<Gone>,
    racy: BTreeSet<u128>,
}

impl Synced {
    fn apply(&mut self, changes: &[Change], snapshot: &Snapshot, root: &Path, held: &[PathBuf]) {
        let run = &self.run;
        let sources: Vec<(&PathBuf, Option<&PathBuf>, Option<&PathBuf>)> = (changes.iter())
            .filter_map(|change| match change {
                Change::Removed { path, .. } => Some((path, None, None)),
                Change::Renamed { from, to, .. } => Some((from, Some(to), None)),
                Change::Ignored { from, to, .. } => Some((from, None, Some(to))),
                _ => None,
            })
            .collect();
        for (from, ..) in &sources {
            assert!(
                self.nodes.contains_key(*from),
                "{from:?} matches nothing in {run}: {changes:?}"
            );
        }
        let disk = on_disk(root);
        let there = |id: &FileId| disk.contains(id);
        for change in changes {
            if let Change::Ignored { to, id, .. } = change {
                let meta = fs::symlink_metadata(root.join(to)).unwrap();
                assert_eq!(
                    u128::from(meta.ino()),
                    id.ino,
                    "{run}: {change:?}"
                );
            }
        }
        let mut lost = Vec::new();
        for (path, node) in std::mem::take(&mut self.nodes) {
            let deepest = (sources.iter())
                .filter(|(from, ..)| path.starts_with(from))
                .max_by_key(|(from, ..)| from.components().count());
            match deepest {
                None => {
                    self.nodes.insert(path, node);
                }
                Some((from, Some(to), _)) => {
                    self.nodes
                        .insert(to.join(path.strip_prefix(from).unwrap()), node);
                }
                Some((_, None, ignored)) => {
                    if ignored.is_none()
                        && there(&node.id)
                        && !self.racy.contains(&node.id.ino)
                    {
                        lost.push((path, node.id));
                    }
                    self.gone.push((node.born, ignored.cloned()));
                }
            }
        }
        for change in changes {
            match change {
                Change::Created { path, .. } => {
                    let node = Node {
                        id: snapshot.entries[path].id,
                        born: (self.step, path.clone()),
                    };
                    let before = self.nodes.insert(path.clone(), node);
                    assert!(
                        before.is_none(),
                        "{path:?} created over {before:?} in {run}"
                    );
                }
                Change::Modified { path } => {
                    let node = self.nodes.get_mut(path);
                    let node = node.unwrap_or_else(|| panic!("{path:?} modified unknown in {run}"));
                    node.id = snapshot.entries[path].id;
                }
                _ => {}
            }
        }
        for (path, id) in lost {
            let at = (snapshot.entries.iter()).find(|(_, entry)| entry.id == id);
            assert!(
                at.is_some_and(|(at, _)| self.nodes.get(at).is_some_and(|node| node.id == id)),
                "{path:?} removed but on disk at {at:?} in {run}: {changes:?}"
            );
        }
        let outside = |path: &Path| !held.iter().any(|root| path.starts_with(root));
        let synced: BTreeMap<PathBuf, FileId> = (self.nodes.iter())
            .filter(|(path, _)| outside(path))
            .map(|(path, node)| (path.clone(), node.id))
            .collect();
        let scanned: BTreeMap<PathBuf, FileId> = (identities(snapshot).into_iter())
            .filter(|(path, _)| outside(path))
            .collect();
        assert_eq!(synced, scanned, "{run}: {changes:?} held {held:?}");
        let ids: Vec<FileId> = snapshot.entries.values().map(|entry| entry.id).collect();
        let unique: BTreeSet<u128> = ids.iter().map(|id| id.ino).collect();
        assert_eq!(
            unique.len(),
            ids.len(),
            "an identity at two paths in {run}: {changes:?}"
        );
    }
}

struct Run {
    snapshot: Snapshot,
    rules: Rules,
    synced: Synced,
}

impl Run {
    fn new(root: &Path, budget: usize, run: String) -> Self {
        let mut rules = rules().with_search_budget(budget);
        let snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
        let nodes = (snapshot.entries.iter())
            .map(|(path, entry)| {
                let born = (0, path.clone());
                (path.clone(), Node { id: entry.id, born })
            })
            .collect();
        let synced = Synced {
            run,
            nodes,
            ..Synced::default()
        };
        Self {
            snapshot,
            rules,
            synced,
        }
    }

    fn rescan(&mut self, root: &Path, paths: Vec<PathBuf>) -> Vec<PathBuf> {
        let rescan = (self.snapshot)
            .rescan_with_rules(root, paths, &mut self.rules)
            .unwrap();
        (self.synced).apply(&rescan.changes, &self.snapshot, root, &rescan.unsettled);
        rescan.unsettled
    }

    fn settle(&mut self, root: &Path) {
        let mut unsettled = self.rescan(root, whole().to_vec());
        for turn in 0..300 {
            if unsettled.is_empty() {
                return;
            }
            let scope = unsettled[turn % unsettled.len()].clone();
            unsettled = self.rescan(root, vec![scope]);
        }
        panic!("the search never ended in {}", self.synced.run);
    }
}

fn small_tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, KOIIGNORE, "ig/\n");
    for n in 0..4 {
        for m in 0..4 {
            write(root, &format!("ig/{n}/{m}.txt"), "x");
        }
    }
    for path in [
        "a/1.txt",
        "a/2.txt",
        "b/1.txt",
        "b/c/1.txt",
        "b/c/2.txt",
        "top.txt",
    ] {
        write(root, path, "y");
    }
    dir
}

fn expect(watcher: &mut dyn Watcher, expected: &[Change], rules: &[RulesFile]) -> Vec<Poll> {
    let deadline = Instant::now() + WAIT;
    let mut polls: Vec<Poll> = Vec::new();
    loop {
        let changes = || polls.iter().flat_map(|poll| &poll.changes);
        let reported = || polls.iter().flat_map(|poll| &poll.rules);
        if expected.iter().all(|change| changes().any(|c| c == change))
            && rules.iter().all(|rule| reported().any(|r| r == rule))
        {
            return polls;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected:?} and {rules:?}; saw {polls:?}"
        );
        polls.push(watcher.poll(Duration::from_millis(500)).unwrap());
    }
}

#[cfg(target_os = "linux")]
fn watched_inodes() -> std::collections::BTreeSet<u64> {
    let mut inodes = std::collections::BTreeSet::new();
    for fd in fs::read_dir("/proc/self/fdinfo").unwrap() {
        let Ok(info) = fs::read_to_string(fd.unwrap().path()) else {
            continue;
        };
        for line in info.lines().filter(|line| line.starts_with("inotify ")) {
            let inode = line
                .split_whitespace()
                .find_map(|field| field.strip_prefix("ino:"))
                .unwrap();
            inodes.insert(u64::from_str_radix(inode, 16).unwrap());
        }
    }
    inodes
}

fn save(root: &Path, path: &str, body: &str) {
    let temp = format!("{path}.tmp");
    write(root, &temp, body);
    fs::rename(root.join(&temp), root.join(path)).unwrap();
}

fn settle(snapshot: &mut Snapshot, root: &Path, rules: &mut Rules) -> Vec<Change> {
    let mut changes = Vec::new();
    for _ in 0..1000 {
        let rescan = snapshot.rescan_with_rules(root, whole(), rules).unwrap();
        changes.extend(rescan.changes);
        if rescan.unsettled.is_empty() {
            return changes;
        }
    }
    panic!("the search never ended: {changes:?}");
}
