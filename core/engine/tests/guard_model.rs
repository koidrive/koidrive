// SPDX-License-Identifier: Apache-2.0

//! The model of another device's mass deletion (docs/design/sync-engine.md section 6.1), checked
//! against the engine over every short sequence of the events that move it.
//!
//! - A remote deletion is one Delete op on the server. It trashes the live part of its node's
//!   subtree, except what the server keeps for a change the deleting device had not seen (R7),
//!   and gives each node it trashes the op's trash group. A node trashed earlier keeps its own
//!   group, so a folder deleted above a deleted one does not take over the inner one's nodes. This device sees a deletion only through its removals, and each removal belongs to
//!   the deletion that trashed the removed node.
//! - Its topmost node is the outermost node this device removes for it. Removing the nodes below
//!   first never changes it, and a deletion above it does not either. It is fully removed when
//!   L holds none of the nodes it trashed.
//! - The window counts every deletion that removed something in the last `WINDOW_MS`, and every
//!   deletion that is not fully removed yet: L still holds a node it trashed. A pause longer
//!   than the window in the middle of a folder's removal does not split the folder.
//! - A deletion ends when the server no longer holds it: it restored all of the nodes, or lost
//!   the commit that trashed them. It then leaves the window and the dismissed deletions at
//!   once. Restoring one of its files leaves the rest of it counted or dismissed.
//!   A purge does not end it: the nodes stay deleted.
//! - A notice opens when the window crosses a limit. While it is open no deletion leaves the
//!   window by age.
//! - Dismissing the notice dismisses the deletions it counted. A later removal of a node one of
//!   them trashed is not counted, until it is fully removed: a rollback that trashes its nodes
//!   again after that is a deletion to count. A removal of a node another deletion trashed is
//!   counted, also under the same folder.
//! - Truncation and restarts change none of this. Every bootstrap after the first lists the
//!   trash, so R holds the trash group of every node it removes.
//!
//! One case is left out: a purge of nodes this device has not removed yet, as after a month
//! offline. R then holds no record of them, and a removal belongs to the deletion the guard
//! counted or dismissed under one of the folders above it, or starts one under the outermost of
//! them. The guard may then count their removals under another deletion, or one deletion as
//! several.
//!
//! The model below keeps this over the server's truth, where each Delete op has an ID no other op
//! shares, also after a loss gives its seq to another commit. Only a resend of the lost commit
//! applies the same Delete again. The server names a trash group after the commit (RFC 0012), so
//! the engine's group stands for that ID; [`Groups::Seq`] names it after the seq as before, for
//! the witnesses of a loss. The engine sees only R, and the test fails when the notice it shows
//! differs from the model's after any event. A loss counts for the model once this device lists R
//! again, as it does for the engine.

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{Hash, Hasher};

use koidrive_engine::classify::Trees;
use koidrive_engine::commit::Commits;
use koidrive_engine::effects::{BatchLimit, Effects};
use koidrive_engine::guard::{WINDOW_MS, Window};
use koidrive_engine::ids::IdGen;
use koidrive_engine::question::{Answer, QuestionKind};
use koidrive_engine::state::{CommitsChanges, TreesChanges};
use koidrive_engine::tree::{Content, FileIdentity, Kind, LocalNode, RemoteNode, Shape};

const ROOT: &str = "root";
const NOW: u64 = 1_794_268_740_000;
/// The most effects one [`Event::Step`] does.
const STEP: usize = 10;
/// The folders and the file another device deletes, restores and purges.
const TARGETS: [&str; 3] = ["p/f", "p", "a0"];
/// A file another device restores on its own, one node of the deletion that trashed it.
const MEMBERS: [&str; 1] = ["p/f/g1"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    /// Another device deletes the node.
    Delete(&'static str),
    /// Another device deletes the folder without having seen an edit of its first file, so the
    /// server keeps that file and the folders above it, and trashes the rest (spec R7).
    DeleteUnseen(&'static str),
    /// Another device restores the node, with its trashed folders.
    Restore(&'static str),
    /// The server purges the node's trash group and everything trashed below it, once this device
    /// has removed all of it: see the module documentation.
    Purge(&'static str),
    /// Another device commits a change elsewhere.
    Commit,
    /// This device does up to [`STEP`] effects, and stops when a notice opens.
    Step,
    /// This device does every effect, and stops when a notice opens.
    StepAll,
    /// Nothing happens for the window's length.
    Pause,
    /// The person dismisses the notice.
    Dismiss,
    /// The log was truncated: R is listed again with the trash, as the driver does once the
    /// folder has synced.
    Bootstrap,
    /// The server lost its last commit. This device does not know yet: it keeps R and the
    /// server's head stays below its `last_seq`, so the next [`Event::Bootstrap`] lists R again
    /// (`AHEAD_OF_SERVER`, design section 4.4).
    Lose,
    /// The device whose Delete the server lost sends that commit again, with the same commit ID,
    /// and the server applies it again.
    Resend,
    /// The engine restarts from its stored state.
    Restart,
}

#[derive(Clone, Debug)]
struct Trash {
    group: String,
    /// The Delete op, unique across rollbacks.
    deletion: u64,
}

#[derive(Clone, Debug)]
struct ServerNode {
    shape: Shape,
    trash: Option<Trash>,
    meta_rev: u64,
}

/// A Delete op the server applied: its target, what it kept, and its ID.
#[derive(Clone, Debug, Hash)]
struct Deleted {
    target: &'static str,
    kept: BTreeSet<String>,
    deletion: u64,
}

/// The state before a commit, for a loss, and the Delete it applied, if any.
#[derive(Clone, Debug)]
struct Undo {
    seq: u64,
    nodes: BTreeMap<String, ServerNode>,
    delete: Option<Deleted>,
}

type Nodes = BTreeMap<String, ServerNode>;

/// Purged nodes, with their kind and the deletion that trashed them.
type Purged = BTreeMap<String, (Kind, u64)>;

#[derive(Clone, Debug, Default)]
struct Server {
    seq: u64,
    nodes: Nodes,
    before: Vec<Undo>,
}

/// How the server names a trash group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Groups {
    /// `<commit_id>:<op index>` (RFC 0012). Each Delete is one commit, so the commit ID is the
    /// Delete's ID, and a resend keeps it.
    Commit,
    /// `<seq>:<op index>`, as before RFC 0012: a commit that takes a lost seq takes its groups.
    Seq,
}

/// The model: the deletions the window counts, with the time of their last removal and their
/// files, the dismissed deletions, and the notice.
#[derive(Clone, Debug, Default, Hash)]
struct Model {
    counted: BTreeMap<u64, (u64, u64)>,
    dismissed: BTreeSet<u64>,
    notice: Option<(u64, u64, u64)>,
}

#[derive(Clone)]
struct World {
    server: Server,
    groups: Groups,
    next_deletion: u64,
    /// The Deletes the server lost, the last one last.
    lost: Vec<Deleted>,
    /// While the server is behind this device: the server's nodes and purged nodes as this
    /// device last saw them.
    seen: Option<(Nodes, Purged)>,
    /// The purged nodes, with their kind and the deletion that trashed them.
    purged: Purged,
    trees: Trees,
    commits: Commits,
    ids: IdGen,
    now: u64,
    last_seq: u64,
    ino: u128,
    model: Model,
}

fn shape(parent: &str, name: &str, kind: Kind) -> Shape {
    Shape {
        parent: parent.into(),
        name: name.into(),
        kind,
        content: (kind == Kind::File).then(|| Content {
            size: 3,
            manifest_digest: vec![1; 32],
            ..Default::default()
        }),
    }
}

fn below(id: &str, top: &str) -> bool {
    id == top || id.starts_with(&format!("{top}/"))
}

impl World {
    /// The space `root`: the files `a0` to `a9`, the folder `p` with the files `p/q0` to
    /// `p/q5`, and the folder `p/f` with the files `p/f/g0` to `p/f/g39`, 56 files.
    fn new(groups: Groups) -> Self {
        let mut nodes = vec![
            ("p".to_owned(), shape(ROOT, "p", Kind::Dir)),
            ("p/f".to_owned(), shape("p", "f", Kind::Dir)),
        ];
        nodes.extend((0..10).map(|i| (format!("a{i}"), shape(ROOT, &format!("a{i}"), Kind::File))));
        nodes.extend((0..6).map(|i| (format!("p/q{i}"), shape("p", &format!("q{i}"), Kind::File))));
        nodes.extend((0..40).map(|i| {
            (
                format!("p/f/g{i}"),
                shape("p/f", &format!("g{i}"), Kind::File),
            )
        }));
        let mut world = World {
            server: Server {
                seq: 1,
                ..Server::default()
            },
            groups,
            next_deletion: 1,
            lost: Vec::new(),
            seen: None,
            purged: BTreeMap::new(),
            trees: Trees::new(ROOT),
            commits: Commits::default(),
            ids: IdGen::new(1),
            now: NOW,
            last_seq: 1,
            ino: 0,
            model: Model::default(),
        };
        for (id, shape) in nodes {
            world.ino += 1;
            world
                .trees
                .set_local(id.clone(), local(shape.clone(), world.ino));
            world.trees.set_synced(id.clone(), Some(shape.clone()));
            let node = ServerNode {
                shape,
                trash: None,
                meta_rev: 1,
            };
            world.trees.set_remote(id.clone(), remote(&node));
            world.server.nodes.insert(id, node);
        }
        world.trees.settle();
        world.trees.take_changes();
        world.commits.take_changes();
        world
    }

    fn take_changes(&mut self, before: &World) -> Result<(), String> {
        let taken = (self.trees.take_changes(), self.commits.take_changes());
        let expected = (
            TreesChanges::between(&before.trees.state(), &self.trees.state()),
            CommitsChanges::between(&before.commits.state(), &self.commits.state()),
        );
        match taken == expected {
            true => Ok(()),
            false => Err(format!("took {taken:?}, the states differ by {expected:?}")),
        }
    }

    fn enabled(&self) -> Vec<Event> {
        let mut events = Vec::new();
        let commits = self.seen.is_none() || self.server.seq + 1 < self.last_seq;
        for target in TARGETS.into_iter().filter(|_| commits) {
            match self
                .server
                .nodes
                .get(target)
                .map(|node| node.trash.is_some())
            {
                Some(false) => {
                    events.push(Event::Delete(target));
                    if self.server.nodes[target].shape.kind == Kind::Dir {
                        events.push(Event::DeleteUnseen(target));
                    }
                }
                Some(true) => {
                    events.push(Event::Restore(target));
                    let purged = self.purge(target);
                    if purged
                        .iter()
                        .all(|(id, _)| self.trees.local().get(id).is_none())
                    {
                        events.push(Event::Purge(target));
                    }
                }
                None => {}
            }
        }
        for member in MEMBERS.into_iter().filter(|_| commits) {
            if (self.server.nodes.get(member)).is_some_and(|node| node.trash.is_some()) {
                events.push(Event::Restore(member));
            }
        }
        if !(Effects::default().plan(
            &self.trees,
            self.commits.in_flight(),
            &mut self.ids.clone(),
            self.now,
            BatchLimit::ONE,
        ))
        .is_empty()
        {
            events.extend([Event::Step, Event::StepAll]);
        }
        if commits {
            events.push(Event::Commit);
            if !self.lost.is_empty() {
                events.push(Event::Resend);
            }
        }
        events.push(Event::Pause);
        if self.notice().is_some() {
            events.push(Event::Dismiss);
        }
        events.push(Event::Bootstrap);
        if !self.server.before.is_empty() {
            events.push(Event::Lose);
        }
        events.push(Event::Restart);
        events
    }

    fn apply(&mut self, event: Event) {
        match event {
            Event::Delete(target) => {
                let deletion = self.next_deletion;
                self.next_deletion += 1;
                self.delete(Deleted {
                    target,
                    kept: BTreeSet::new(),
                    deletion,
                });
            }
            Event::DeleteUnseen(target) => {
                let nodes = &self.server.nodes;
                let edited = (nodes.iter()).find(|(id, node)| {
                    below(id, target) && node.trash.is_none() && node.shape.kind == Kind::File
                });
                let mut kept = BTreeSet::new();
                let mut id = edited.map(|(id, _)| id.clone());
                while let Some(node) = id.filter(|id| below(id, target)) {
                    id = Some(nodes[&node].shape.parent.clone());
                    kept.insert(node);
                }
                let deletion = self.next_deletion;
                self.next_deletion += 1;
                self.delete(Deleted {
                    target,
                    kept,
                    deletion,
                });
            }
            Event::Restore(target) => {
                let changed = self.commit(|server| {
                    let mut changed = Vec::new();
                    revive(server, target, &mut changed);
                    changed
                });
                self.pull(changed);
            }
            Event::Purge(target) => {
                let purged = self.purge(target);
                self.commit(|server| {
                    for (id, _) in &purged {
                        server.nodes.remove(id);
                    }
                });
                for (id, node) in &purged {
                    let deletion = node.trash.as_ref().unwrap().deletion;
                    self.purged.insert(id.clone(), (node.shape.kind, deletion));
                }
                self.pull(purged.into_iter().map(|(id, _)| id).collect());
            }
            Event::Commit => {
                self.commit(|_| ());
                self.pull(Vec::new());
            }
            Event::Step => self.step(STEP),
            Event::StepAll => self.step(usize::MAX),
            Event::Pause => self.now += WINDOW_MS,
            Event::Dismiss => {
                let id = self.notice().unwrap().0;
                assert!(self.commits.decide(&id, Answer::Dismiss));
                let model = &mut self.model;
                model
                    .dismissed
                    .extend(std::mem::take(&mut model.counted).into_keys());
                model.notice = None;
            }
            Event::Bootstrap => self.bootstrap(),
            Event::Lose => {
                if self.seen.is_none() {
                    self.seen = Some((self.server.nodes.clone(), self.purged.clone()));
                }
                let undo = self.server.before.pop().unwrap();
                self.server.seq = undo.seq;
                self.server.nodes = undo.nodes;
                self.lost.extend(undo.delete);
                let server = &self.server;
                self.purged.retain(|id, _| !server.nodes.contains_key(id));
            }
            Event::Resend => {
                let deleted = self.lost.pop().unwrap();
                self.delete(deleted);
            }
            Event::Restart => {
                self.commits = Commits::from_state(self.commits.state()).unwrap();
            }
        }
        let trashing = self.trashing();
        self.model
            .counted
            .retain(|deletion, _| trashing.contains(deletion));
        let unfinished: BTreeSet<u64> = (self.trees.local().iter())
            .filter_map(|(id, _)| self.truth(id).map(|(_, deletion)| deletion))
            .collect();
        (self.model.dismissed)
            .retain(|deletion| trashing.contains(deletion) && unfinished.contains(deletion));
        let built = (self
            .commits
            .build(&mut self.trees, &mut self.ids, self.now, "h", "me", 7))
        .is_some();
        assert!(!built, "this device changed nothing");
        self.trees.settle();
    }

    /// The nodes a purge of `target` removes: those in its trash group and everything trashed
    /// below them.
    fn purge(&self, target: &str) -> Vec<(String, ServerNode)> {
        let group = &self.server.nodes[target].trash.as_ref().unwrap().group;
        let tops: Vec<&String> = (self.server.nodes.iter())
            .filter(|(_, node)| node.trash.as_ref().is_some_and(|t| t.group == *group))
            .map(|(id, _)| id)
            .collect();
        (self.server.nodes.iter())
            .filter(|(id, node)| node.trash.is_some() && tops.iter().any(|top| below(id, top)))
            .map(|(id, node)| (id.clone(), node.clone()))
            .collect()
    }

    /// Another device's Delete: the server trashes `target` and keeps `kept`.
    fn delete(&mut self, deleted: Deleted) {
        let Deleted {
            target,
            ref kept,
            deletion,
        } = deleted;
        let groups = self.groups;
        let changed = self.commit(|server| {
            let group = match groups {
                Groups::Commit => format!("c{deletion}:0"),
                Groups::Seq => format!("{}:0", server.seq),
            };
            let trash = Trash { group, deletion };
            let mut changed = Vec::new();
            for (id, node) in &mut server.nodes {
                if below(id, target) && node.trash.is_none() && !kept.contains(id) {
                    node.trash = Some(trash.clone());
                    node.meta_rev = server.seq;
                    changed.push(id.clone());
                }
            }
            changed
        });
        self.server.before.last_mut().unwrap().delete = Some(deleted);
        self.pull(changed);
    }

    fn commit<T>(&mut self, change: impl FnOnce(&mut Server) -> T) -> T {
        let server = &mut self.server;
        server.before.push(Undo {
            seq: server.seq,
            nodes: server.nodes.clone(),
            delete: None,
        });
        server.seq += 1;
        change(server)
    }

    /// R takes the changes of the commits since `last_seq`, unless the server is behind it.
    fn pull(&mut self, changed: Vec<String>) {
        if self.seen.is_some() {
            return;
        }
        for id in changed {
            match self.server.nodes.get(&id) {
                Some(node) => self.trees.set_remote(id, remote(node)),
                None => self.trees.remove_remote(&id),
            }
        }
        self.trees.settle();
        self.last_seq = self.server.seq;
    }

    fn bootstrap(&mut self) {
        let held: Vec<String> = (self.trees.remote().iter())
            .map(|(id, _)| id.clone())
            .filter(|id| id != ROOT)
            .collect();
        for id in held {
            self.trees.remove_remote(&id);
        }
        for (id, node) in &self.server.nodes {
            self.trees.set_remote(id.clone(), remote(node));
        }
        self.trees.settle();
        self.last_seq = self.server.seq;
        self.seen = None;
    }

    fn step(&mut self, limit: usize) {
        self.now += 1;
        let noticed = self.notice().is_some();
        let mut effects = Effects::default();
        for _ in 0..limit {
            let Some(record) = effects
                .plan(
                    &self.trees,
                    self.commits.in_flight(),
                    &mut self.ids,
                    self.now,
                    BatchLimit::ONE,
                )
                .first()
                .cloned()
            else {
                break;
            };
            self.ino += 1;
            let identity = Some(identity(self.ino));
            let removal = effects
                .done(
                    &mut self.trees,
                    self.commits.in_flight(),
                    &record.effect_id,
                    identity,
                )
                .unwrap();
            if let Some(removal) = removal {
                let node = &removal.within[0];
                let (kind, deletion) = self.truth(node).expect("a removed node is trashed");
                let files = u64::from(kind == Kind::File);
                assert_eq!(removal.files, files);
                assert!(
                    (self.trees.remote().get(node)).is_some_and(|node| node.trashed),
                    "R holds the trash group of every removed node"
                );
                let unfinished: BTreeSet<u64> = (self.trees.local().iter())
                    .filter_map(|(id, _)| self.truth(id).map(|(_, deletion)| deletion))
                    .collect();
                let trashing = self.trashing();
                let synced_files = self.trees.synced_files();
                let model = &mut self.model;
                model.count(
                    deletion,
                    files,
                    &unfinished,
                    &trashing,
                    synced_files,
                    self.now,
                );
                (self.commits).count_remote(
                    removal,
                    &mut self.trees,
                    self.last_seq,
                    &mut self.ids,
                    self.now,
                );
            }
            self.trees.settle();
            if !noticed && self.notice().is_some() {
                break;
            }
        }
    }

    /// The server's nodes and purged nodes as this device knows them: a loss counts only once
    /// this device lists R again.
    fn known(&self) -> (&Nodes, &Purged) {
        match &self.seen {
            Some((nodes, purged)) => (nodes, purged),
            None => (&self.server.nodes, &self.purged),
        }
    }

    /// Removes every node L holds and R does not, counting each removal in the engine alone. The
    /// model leaves out removals of nodes the server purged first, so this checks nothing.
    fn step_past_the_model(&mut self) {
        self.now += 1;
        let mut effects = Effects::default();
        while let Some(record) = effects
            .plan(
                &self.trees,
                self.commits.in_flight(),
                &mut self.ids,
                self.now,
                BatchLimit::ONE,
            )
            .first()
            .cloned()
        {
            self.ino += 1;
            let identity = Some(identity(self.ino));
            let removal = (effects.done(
                &mut self.trees,
                self.commits.in_flight(),
                &record.effect_id,
                identity,
            ))
            .unwrap();
            if let Some(removal) = removal {
                (self.commits).count_remote(
                    removal,
                    &mut self.trees,
                    self.last_seq,
                    &mut self.ids,
                    self.now,
                );
            }
            self.trees.settle();
        }
    }

    /// The deletions the server holds: those whose nodes it has not restored or lost.
    fn trashing(&self) -> BTreeSet<u64> {
        let (nodes, purged) = self.known();
        (nodes.values())
            .filter_map(|node| node.trash.as_ref().map(|trash| trash.deletion))
            .chain(purged.values().map(|(_, deletion)| *deletion))
            .collect()
    }

    /// The kind of a node the server trashed, and the deletion that trashed it.
    fn truth(&self, id: &str) -> Option<(Kind, u64)> {
        let (nodes, purged) = self.known();
        match nodes.get(id) {
            Some(node) => (node.trash.as_ref()).map(|trash| (node.shape.kind, trash.deletion)),
            None => purged.get(id).copied(),
        }
    }

    /// The engine's notice: its ID and its counts.
    fn notice(&self) -> Option<(String, Window)> {
        self.commits
            .questions()
            .iter()
            .find_map(|question| match &question.kind {
                QuestionKind::RemoteMassDelete {
                    deletions,
                    affected_files,
                    space_files,
                    ..
                } => Some((
                    question.id.clone(),
                    Window {
                        deletions: *deletions,
                        affected_files: *affected_files,
                        space_files: *space_files,
                    },
                )),
                _ => None,
            })
    }

    fn check(&self) -> Result<(), String> {
        let engine = self
            .notice()
            .map(|(_, window)| (window.deletions, window.affected_files, window.space_files));
        if engine == self.model.notice {
            return Ok(());
        }
        Err(format!(
            "the engine's notice (deletions, files, space) is {engine:?}, the model's {:?}\n  \
             model: {:?}\n  engine guard: {:?}",
            self.model.notice,
            self.model,
            self.commits.state().guard
        ))
    }

    fn key(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.model.hash(&mut hasher);
        (self.now, self.last_seq, self.next_deletion, &self.lost).hash(&mut hasher);
        let state = (
            &self.server,
            &self.seen,
            &self.purged,
            &self.trees,
            self.commits.state(),
        );
        format!("{state:?}").hash(&mut hasher);
        hasher.finish()
    }
}

impl Model {
    /// Counts a removal. `unfinished` are the deletions L still holds a node of, and `trashing`
    /// the deletions whose nodes the server has not restored or lost.
    fn count(
        &mut self,
        deletion: u64,
        files: u64,
        unfinished: &BTreeSet<u64>,
        trashing: &BTreeSet<u64>,
        synced_files: u64,
        now: u64,
    ) {
        if self.dismissed.contains(&deletion) {
            return;
        }
        self.counted.retain(|other, _| trashing.contains(other));
        if self.notice.is_none() {
            self.counted.retain(|other, (at, _)| {
                *other == deletion || unfinished.contains(other) || *at + WINDOW_MS > now
            });
        }
        let counted = self.counted.entry(deletion).or_insert((now, 0));
        *counted = (now, counted.1 + files);
        let affected_files = self.counted.values().map(|(_, files)| files).sum();
        let window = Window {
            deletions: self.counted.len() as u64,
            affected_files,
            space_files: synced_files + affected_files,
        };
        if self.notice.is_some() || !window.exceeded().is_empty() {
            self.notice = Some((window.deletions, window.affected_files, window.space_files));
        }
    }
}

/// Revives a trashed node with the nodes below it in its trash group, and each trashed folder
/// above it on its own, as a restore does on the server (`reviveTarget` in
/// server/src/space/commit.ts).
fn revive(server: &mut Server, id: &str, changed: &mut Vec<String>) {
    let mut above = Vec::new();
    let mut parent = server.nodes[id].shape.parent.clone();
    while (server.nodes.get(&parent)).is_some_and(|node| node.trash.is_some()) {
        above.push(parent.clone());
        parent = server.nodes[&parent].shape.parent.clone();
    }
    for folder in above.into_iter().rev() {
        let node = server.nodes.get_mut(&folder).unwrap();
        node.trash = None;
        node.meta_rev = server.seq;
        changed.push(folder);
    }
    let Some(trash) = server.nodes[id].trash.clone() else {
        return;
    };
    let mut todo = vec![id.to_owned()];
    while let Some(id) = todo.pop() {
        let node = server.nodes.get_mut(&id).unwrap();
        node.trash = None;
        node.meta_rev = server.seq;
        changed.push(id.clone());
        todo.extend(
            (server.nodes.iter())
                .filter(|(_, child)| {
                    child.shape.parent == id
                        && child.trash.as_ref().is_some_and(|t| t.group == trash.group)
                })
                .map(|(child, _)| child.clone()),
        );
    }
}

fn remote(node: &ServerNode) -> RemoteNode {
    RemoteNode {
        shape: node.shape.clone(),
        trashed: node.trash.is_some(),
        content_rev: 1,
        meta_rev: node.meta_rev,
        meta_edit_rev: node.meta_rev,
        trash_group: (node.trash.as_ref()).map_or_else(String::new, |t| t.group.clone()),
        version_id: String::new(),
        edit_rev: 1,
        pending_for: None,
        pending_by: None,
    }
}

fn local(shape: Shape, ino: u128) -> LocalNode {
    LocalNode {
        shape,
        identity: identity(ino),
        parked: false,
    }
}

fn identity(ino: u128) -> FileIdentity {
    FileIdentity {
        dev: 1,
        ino,
        birth_ns: Some(0),
    }
}

/// Every sequence of up to `depth` events, skipping a state already explored with at least as many
/// events left. Returns the failures.
fn explore(
    world: &World,
    path: &mut Vec<Event>,
    depth: usize,
    seen: &mut HashMap<u64, usize>,
    failures: &mut Vec<String>,
) {
    for event in world.enabled() {
        let mut next = world.clone();
        next.apply(event);
        path.push(event);
        match next.take_changes(world).and_then(|()| next.check()) {
            Err(error) => failures.push(format!("{path:?}: {error}")),
            Ok(()) if depth > 1 && seen.get(&next.key()).is_none_or(|&left| left < depth - 1) => {
                seen.insert(next.key(), depth - 1);
                explore(&next, path, depth - 1, seen, failures)
            }
            Ok(()) => {}
        }
        path.pop();
    }
}

#[test]
fn the_notice_follows_the_model_over_every_short_sequence_of_events() {
    let depth = (std::env::var("GUARD_MODEL_DEPTH").ok()).map_or(5, |depth| depth.parse().unwrap());
    for depth in 1..=depth {
        let (mut seen, mut failures) = (HashMap::new(), Vec::new());
        seen.insert(World::new(Groups::Commit).key(), depth);
        explore(
            &World::new(Groups::Commit),
            &mut Vec::new(),
            depth,
            &mut seen,
            &mut failures,
        );
        failures.sort_by_key(String::len);
        if std::env::var("GUARD_MODEL_ALL").is_ok() {
            for failure in &failures {
                eprintln!("{}", failure.lines().next().unwrap());
            }
        }
        assert!(
            failures.is_empty(),
            "{} sequences of {depth} events differ from the model, the first:\n{}",
            failures.len(),
            failures[..failures.len().min(3)].join("\n")
        );
        eprintln!("depth {depth}: {} states", seen.len());
    }
}

/// Runs `events`, each compared with the model, and returns the engine's notice at the end:
/// deletions, files and space.
fn witness(events: &[Event]) -> Option<(u64, u64, u64)> {
    run(events)
        .notice()
        .map(|(_, window)| (window.deletions, window.affected_files, window.space_files))
}

/// Runs `events`, each compared with the model, and returns the world they leave.
fn run(events: &[Event]) -> World {
    try_run(Groups::Commit, events).unwrap_or_else(|error| panic!("{error}"))
}

/// Runs `events` with trash groups named by `groups`, and returns the world they leave, or the
/// first difference from the model.
fn try_run(groups: Groups, events: &[Event]) -> Result<World, String> {
    let mut world = World::new(groups);
    for (index, event) in events.iter().enumerate() {
        assert!(world.enabled().contains(event), "{event:?} at {index}");
        let before = world.clone();
        world.apply(*event);
        (world.take_changes(&before).and_then(|()| world.check()))
            .map_err(|error| format!("{:?}: {error}", &events[..=index]))?;
    }
    Ok(world)
}

use Event::{
    Bootstrap, Commit, Delete, DeleteUnseen, Dismiss, Lose, Pause, Resend, Restore, Step, StepAll,
};

/// Removals until the notice about `p/f` opens, 29 of its 40 files, and the dismissal.
const DISMISSED_MID_FOLDER: [Event; 5] = [Delete("p/f"), Step, Step, Step, Dismiss];

/// GUN-384 review: the rest of a folder removed after its notice was dismissed is not noticed.
#[test]
fn witness_the_rest_of_a_dismissed_folder() {
    assert_eq!(
        witness(&[&DISMISSED_MID_FOLDER[..], &[StepAll]].concat()),
        None
    );
}

/// GUN-384 review: a folder deleted above a dismissed one counts only its own 6 files.
#[test]
fn witness_a_folder_deleted_above_a_dismissed_one() {
    let events = [&DISMISSED_MID_FOLDER[..], &[Delete("p"), StepAll]].concat();
    assert_eq!(witness(&events), None);
}

/// GUN-384 review: 20 of the 40 files of `p/f`, a pause past the window, and the other 20.
#[test]
fn witness_a_folder_removal_that_stops_past_the_window() {
    let events = [Delete("p/f"), Step, Step, Pause, StepAll, StepAll];
    assert_eq!(witness(&events), Some((1, 40, 56)));
}

/// As above, and a file deleted after the pause is removed before the rest of the folder. The
/// folder still counts its first 20 files.
#[test]
fn witness_another_deletion_after_a_folder_removal_stopped_past_the_window() {
    let events = [
        Delete("p/f"),
        Step,
        Step,
        Pause,
        Delete("a0"),
        StepAll,
        StepAll,
    ];
    assert_eq!(witness(&events), Some((2, 41, 56)));
}

/// GUN-384 review: a folder restored after the dismissal and deleted again is noticed.
#[test]
fn witness_a_folder_restored_and_deleted_again() {
    let again = [Restore("p/f"), StepAll, Delete("p/f"), StepAll, StepAll];
    let events = [&DISMISSED_MID_FOLDER[..], &again].concat();
    assert_eq!(witness(&events), Some((1, 40, 56)));
}

/// GUN-384 review: a bootstrap after the dismissal does not reopen the folder.
#[test]
fn witness_a_bootstrap_after_the_dismissal() {
    let events = [&DISMISSED_MID_FOLDER[..], &[Bootstrap, StepAll]].concat();
    assert_eq!(witness(&events), None);
}

/// GUN-384 review: R holds no record of the folder while it is removed and dismissed, then a
/// commit elsewhere, a rollback of it, and the list after it tell R the folder's group.
#[test]
fn witness_a_dismissal_without_the_record_then_a_rollback() {
    let events = [
        Delete("p/f"),
        Bootstrap,
        Step,
        Step,
        Step,
        Dismiss,
        Commit,
        Lose,
        Bootstrap,
        StepAll,
    ];
    assert_eq!(witness(&events), None);
}

/// GUN-384 review: as above, and the folder is restored and deleted again across two more
/// truncations. The second deletion is noticed.
#[test]
fn witness_a_new_deletion_after_a_second_truncation() {
    let events = [
        Delete("p/f"),
        Bootstrap,
        Step,
        Step,
        Step,
        Dismiss,
        Bootstrap,
        Restore("p/f"),
        StepAll,
        Bootstrap,
        Delete("p/f"),
        StepAll,
        StepAll,
    ];
    assert_eq!(witness(&events), Some((1, 40, 56)));
}

/// GUN-384 review: the server keeps one edited file of `p` and the folders above it, so the
/// 45 files it trashes share no folder. The 16 left after the dismissal are not noticed again.
#[test]
fn witness_the_rest_of_a_delete_the_server_applied_in_part() {
    let events = [DeleteUnseen("p"), Step, Step, Step, Dismiss, StepAll];
    assert_eq!(witness(&events), None);
}

/// GUN-384 review: one file restored from a folder whose notice was dismissed part way through its
/// removal. The server brings back the folder on its own and the file, so the other 39 files stay
/// deleted and dismissed.
#[test]
fn witness_one_file_restored_from_a_dismissed_folder() {
    let world = run(&[&DISMISSED_MID_FOLDER[..], &[Restore("p/f/g1"), StepAll]].concat());
    assert_eq!(world.notice().map(|(_, window)| window.deletions), None);
    let trashed = (world.server.nodes.iter())
        .filter(|(id, node)| below(id, "p/f") && node.trash.is_some())
        .map(|(id, _)| id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(trashed.len(), 39, "{trashed:?}");
    assert!(!trashed.contains(&"p/f") && !trashed.contains(&"p/f/g1"));
}

/// GUN-384 review: a delete the server applied in part, then a delete of the folders it kept,
/// then one file of the first restored. The server brings back p and p/f on their own, so L holds
/// the second delete's topmost node again while its file stays trashed. This device has removed
/// all of the second delete, so it leaves the window after the pause, and the notice counts only
/// the rest of the first.
#[test]
fn witness_a_delete_whose_folder_a_restore_brought_back_leaves_the_window() {
    let events = [
        DeleteUnseen("p"),
        Delete("p"),
        Step,
        Restore("p/f/g1"),
        Pause,
        StepAll,
    ];
    assert_eq!(witness(&events), Some((1, 29, 56)));
}

/// GUN-455: a folder deleted, one of its folders restored, which brings the folder back, and the
/// folder deleted again. This device removed all of the first delete before the pause, so the
/// first delete leaves the window, and the notice counts only the second delete's 26 files.
#[test]
fn witness_a_folder_a_restore_brought_back_and_deleted_again_counts_only_the_new_delete() {
    let events = [
        Delete("p"),
        Step,
        Restore("p/f"),
        StepAll,
        Delete("p"),
        Pause,
        StepAll,
    ];
    assert_eq!(witness(&events), Some((1, 26, 50)));
}

/// GUN-455: as the witness above for a delete the server applied in part. Once the restore brought
/// p back and p was deleted again, the second delete's one removal is all of it, so it leaves the
/// window, and the notice counts only the restored file's delete.
#[test]
fn witness_a_delete_whose_folder_was_deleted_again_after_a_restore_leaves_the_window() {
    let events = [
        DeleteUnseen("p"),
        Delete("p"),
        Step,
        Restore("p/f/g1"),
        Delete("p"),
        Pause,
        StepAll,
    ];
    assert_eq!(witness(&events), Some((1, 28, 55)));
}

/// GUN-455: a folder removed and noticed, one of its folders restored, the notice dismissed, and
/// the restore lost. The restore had brought back all that was left of the delete, so the
/// dismissal ended with it, and removing the folder again after the loss is noticed.
#[test]
fn witness_a_restore_lost_after_the_dismissal_is_noticed() {
    let events = [
        Delete("p"),
        StepAll,
        Restore("p/f"),
        StepAll,
        Dismiss,
        Lose,
        Bootstrap,
        StepAll,
    ];
    assert_eq!(witness(&events), Some((1, 26, 50)));
}

/// GUN-455 review: a dismissed folder brought back by a restore, then deleted again and purged
/// before this device builds or removes anything. The restore ended the dismissal, so the
/// removals, of nodes R holds no record of, are noticed with all 40 files of p/f. The model leaves
/// such removals out, so only the engine counts them.
#[test]
fn a_new_delete_of_a_dismissed_folder_purged_before_its_removal_is_noticed() {
    let mut world = World::new(Groups::Commit);
    for event in [Delete("p"), StepAll, Dismiss, Restore("p/f"), StepAll] {
        world.apply(event);
    }
    let deletion = world.next_deletion;
    world.next_deletion += 1;
    world.delete(Deleted {
        target: "p",
        kept: BTreeSet::new(),
        deletion,
    });
    world.apply(Event::Purge("p"));
    world.step_past_the_model();
    let notice = (world.notice()).map(|(_, w)| (w.deletions, w.affected_files, w.space_files));
    assert_eq!(notice, Some((1, 40, 50)));
}

/// GUN-384 review: one file restored from a delete the server applied in part, after the notice
/// was dismissed part way through its removal. The rest of the delete stays dismissed, and the
/// folder deleted above it counts only its own 8 files.
#[test]
fn witness_one_file_restored_from_a_dismissed_delete() {
    let events = [
        DeleteUnseen("p/f"),
        Step,
        Step,
        Step,
        Dismiss,
        Restore("p/f/g1"),
        Delete("p"),
        StepAll,
    ];
    assert_eq!(witness(&events), None);
}

/// Runs `events`, which reach a Delete that takes the seq of a lost one, and checks the notice
/// at the end. With trash groups named after the seq, the new Delete takes the lost one's group,
/// and the engine's notice differs from the model's.
fn witness_across_a_loss(events: &[Event], notice: Option<(u64, u64, u64)>) {
    assert_eq!(witness(events), notice);
    assert!(
        try_run(Groups::Seq, events).is_err(),
        "with groups named after the seq, the engine follows the model"
    );
}

/// GUN-404: `p` is dismissed part way through its removal, the server loses that Delete and the
/// commit after it, and another Delete of `p` takes the lost seq. The new Delete is noticed for
/// the 17 files L still holds.
#[test]
fn witness_a_dismissed_folder_deleted_again_at_a_lost_seq() {
    let events = [
        Delete("p"),
        Step,
        Step,
        Step,
        Dismiss,
        Commit,
        Lose,
        Lose,
        Delete("p"),
        Bootstrap,
        StepAll,
        StepAll,
    ];
    witness_across_a_loss(&events, Some((1, 17, 27)));
}

/// GUN-404: as above with `p/f` dismissed and `p` deleted at the lost seq, so the new Delete is
/// of another folder.
#[test]
fn witness_a_folder_above_a_dismissed_one_deleted_at_a_lost_seq() {
    let events = [
        Delete("p/f"),
        Step,
        Step,
        Step,
        Dismiss,
        Commit,
        Lose,
        Lose,
        Delete("p"),
        Bootstrap,
        StepAll,
        StepAll,
    ];
    witness_across_a_loss(&events, Some((1, 17, 27)));
}

/// GUN-404: 10 files of `p` are removed, below the limits, and the server loses that Delete. The
/// lost Delete ends once R is listed again, so the new Delete of `p` counts only its own 36
/// files. With the lost group, the two are one deletion of 46 files.
#[test]
fn witness_a_counted_folder_deleted_again_at_a_lost_seq() {
    let events = [
        Delete("p"),
        Step,
        Commit,
        Lose,
        Lose,
        Delete("p"),
        Bootstrap,
        StepAll,
        StepAll,
    ];
    witness_across_a_loss(&events, Some((1, 36, 46)));
}

/// GUN-404: as above with 10 files of `p/f` removed and `p` deleted at the lost seq.
#[test]
fn witness_a_folder_above_a_counted_one_deleted_at_a_lost_seq() {
    let events = [
        Delete("p/f"),
        Step,
        Commit,
        Lose,
        Lose,
        Delete("p"),
        Bootstrap,
        StepAll,
        StepAll,
    ];
    witness_across_a_loss(&events, Some((1, 36, 46)));
}

/// GUN-404, RFC 0012: the server loses a dismissed Delete, and its device sends the commit again
/// with the same commit ID. The Delete keeps its group, so it stays dismissed.
#[test]
fn witness_a_dismissed_delete_sent_again_after_a_loss() {
    let events = [
        Delete("p"),
        Step,
        Step,
        Step,
        Dismiss,
        Commit,
        Lose,
        Lose,
        Resend,
        Bootstrap,
        StepAll,
    ];
    assert_eq!(witness(&events), None);
}
