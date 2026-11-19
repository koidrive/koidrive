// SPDX-License-Identifier: Apache-2.0

//! One device of the simulation: its disk, the engine state, the copy of that state the store
//! holds, and the connection to the server.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Debug;

use koidrive_engine::classify::Trees;
use koidrive_engine::commit::{Base, Commits, GIVEN_UP_KEPT, Halt};
use koidrive_engine::effects::Effects;
use koidrive_engine::first_link::History;
use koidrive_engine::ids::IdGen;
use koidrive_engine::ignore::{LocalRules, RuleFile, Source};
use koidrive_engine::names::name_key;
use koidrive_engine::state::{CommitsChanges, EffectsChanges, TreesChanges};
use koidrive_engine::tree::{Kind, LocalNode, NodeId, RemoteNode, Shape};
use koidrive_proto::v1::{
    ChangeRecord, CommitRecord, CommitResult, Error, NodeState, Resume, change_record,
};

use super::disk::{Disk, Place, identity};
use super::{Failure, NOW, RULES_FILE};
use crate::space::Session;

/// Everything the store keeps for a space (docs/design/sync-engine.md section 2.8).
#[derive(Clone, Debug, PartialEq)]
pub struct Engine {
    pub trees: Trees,
    pub commits: Commits,
    pub effects: Effects,
    pub ids: IdGen,
    /// The highest seq up to which R holds every record, with the commit token there (RFC 0013).
    pub cursor: Base,
    /// The server lost commits that R holds, so R waits for a listing of the server's tree and
    /// nothing is planned until it comes (section 4.4).
    pub behind: bool,
    pub history: String,
}

impl Engine {
    /// Records that the server lost commits of this device, with the edits of the commit in flight
    /// and `dropped`, those of one the loss already dropped.
    pub fn server_behind(&mut self, dropped: Vec<NodeId>) {
        let in_flight = self.commits.edited_in_flight();
        self.trees
            .record_dropped_edits(dropped.into_iter().chain(in_flight));
        self.behind = true;
    }

    pub fn new(root: &str, history: &str, seed: u64) -> Self {
        Self {
            trees: Trees::new(root),
            commits: Commits::default(),
            effects: Effects::default(),
            ids: IdGen::new(seed),
            cursor: Base::default(),
            behind: false,
            history: history.to_owned(),
        }
    }

    /// Rebuilds the engine the way the store loads it after a restart, and checks that every part
    /// comes back equal. The store does not hold the ignore rules, so the device reads `rules`
    /// again, and a restart plans nothing before its full scan, so the rules count as read.
    pub fn reload(&self, rules: &Rules) -> Result<Engine, Failure> {
        let mut trees = Trees::new(self.trees.remote().root.clone());
        for (id, node) in self.trees.remote().iter() {
            trees.set_remote(id.clone(), node.clone());
        }
        for (id, node) in self.trees.local().iter() {
            trees.set_local(id.clone(), node.clone());
        }
        for (id, shape) in self.trees.synced().iter() {
            trees.set_synced(id.clone(), Some(shape.clone()));
            let mark = self.trees.synced_mark(id).map(str::to_owned);
            trees.set_synced_mark(id.clone(), mark);
        }
        let refused = |error| Failure::Broken(format!("the stored state was refused: {error:?}"));
        trees.restore(self.trees.state()).map_err(refused)?;
        rules.read(&mut trees);
        let commits = Commits::from_state(self.commits.state()).map_err(refused)?;
        commits.hold(&mut trees);
        let loaded = Engine {
            trees,
            commits,
            effects: Effects::from_state(self.effects.state()).map_err(refused)?,
            ids: IdGen::new(self.ids.state),
            cursor: self.cursor.clone(),
            behind: self.behind,
            history: self.history.clone(),
        };
        if loaded != *self {
            return Err(Failure::Broken(
                "the stored state did not come back equal".into(),
            ));
        }
        Ok(loaded)
    }

    /// Applies change records to R, as the receiving side does for records in seq order.
    pub fn apply(&mut self, changes: &[ChangeRecord], made: Option<&str>) -> Result<(), Failure> {
        for change in changes {
            match &change.change {
                Some(change_record::Change::Node(state))
                    if state.node_id != self.trees.remote().root =>
                {
                    let (id, node) = RemoteNode::from_state(state).map_err(|error| {
                        Failure::Broken(format!("unreadable node state: {error:?}"))
                    })?;
                    if made.is_some_and(|commit_id| self.commits.answers_in_flight(commit_id, &id))
                    {
                        self.trees.set_remote_answered_here(id, node);
                    } else if made
                        .is_some_and(|commit_id| self.commits.edits_in_flight(commit_id, &id))
                    {
                        self.trees.set_remote_edited_here(id, node);
                    } else if made.is_some() {
                        self.trees.set_remote_made_here(id, node);
                    } else {
                        self.trees.set_remote(id, node);
                    }
                }
                Some(change_record::Change::Purged(purged)) => {
                    self.trees.remove_remote(&purged.node_id)
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// A full rescan: L takes the disk as it is. An entry keeps the node whose file identity it
    /// has, an entry with an unknown identity becomes a new node, and a node whose entry is gone
    /// leaves L. A parked entry keeps the position L holds for it and takes the content it has.
    pub fn scan(&mut self, disk: &Disk) {
        self.scan_at(disk, &[]);
    }

    /// A rescan of the folder at `scope`, as a watcher does for a folder its events name, which
    /// takes the disk as [`Engine::scan`] does for every entry it reaches. Like
    /// `koidrive_fs::Snapshot::rescan`, it widens a place to its parent until L holds the folder the
    /// disk has at the parent, lists that parent, and reaches the place, every name L does not hold
    /// there and every name L holds that the listing lacks, each with everything below it. It also
    /// reaches both places of every entry whose identity crosses the edge of what it reaches, so a
    /// move into or out of the scope stays one node, and it widens to the whole disk when one of
    /// them is in the parked directory. The rest of L keeps its state, also where the disk changed.
    pub fn scan_at(&mut self, disk: &Disk, scope: &[String]) -> Reached {
        let Reach {
            places,
            widened,
            entries,
            nodes: reached,
        } = self.reach(disk, scope);
        let observed: BTreeSet<u128> = (entries.iter().copied())
            .chain(
                (reached.iter())
                    .filter_map(|id| self.trees.local().get(id))
                    .map(|node| node.identity.ino),
            )
            .collect();
        let root = self.trees.local().root.clone();
        let known: BTreeMap<u128, NodeId> = (self.trees.local().iter())
            .map(|(id, node)| (node.identity.ino, id.clone()))
            .collect();
        let displaced: BTreeMap<u128, NodeId> = (self.trees.displaced_local().iter())
            .map(|(id, identity)| (identity.ino, id.clone()))
            .collect();
        let mut nodes: BTreeMap<u128, NodeId> = BTreeMap::new();
        let mut queue: VecDeque<u128> = (entries.iter().copied())
            .filter(|ino| {
                !matches!(&disk.entries()[ino].place, Place::In(parent) if entries.contains(parent))
            })
            .collect();
        let mut left_out: BTreeSet<NodeId> = BTreeSet::new();
        let mut placed: BTreeSet<NodeId> = BTreeSet::new();
        while let Some(ino) = queue.pop_front() {
            let entry = &disk.entries()[&ino];
            let kind = match entry.content {
                Some(_) => Kind::File,
                None => Kind::Dir,
            };
            if disk
                .path(ino)
                .is_some_and(|path| self.trees.ignores(&path, kind))
            {
                queue.extend(disk.children(&Place::In(ino)));
                if let Some(id) = known.get(&ino) {
                    left_out.insert(id.clone());
                    if self.leave_out(disk, (ino, id), &nodes, &known) {
                        placed.insert(id.clone());
                    }
                }
                continue;
            }
            let marked = (entry.mark.as_ref())
                .filter(|_| !matches!(self.commits.halted(), Some(Halt::Recovery(_))))
                .filter(|mark| mark.ino == ino)
                .map(|mark| mark.node.clone())
                .filter(|node| {
                    self.trees.local().get(node).is_none()
                        && self.trees.synced().get(node).is_none()
                        && self.trees.remote().get(node).is_none()
                        && !nodes.values().any(|taken| taken == node)
                });
            let id = match known.get(&ino).or_else(|| displaced.get(&ino)) {
                Some(id) => id.clone(),
                None => marked.unwrap_or_else(|| self.ids.uuid_v7(NOW as u64)),
            };
            nodes.insert(ino, id.clone());
            queue.extend(disk.children(&Place::In(ino)));
            let parent = match &entry.place {
                Place::Root => root.clone(),
                Place::In(parent) => nodes.get(parent).unwrap_or_else(|| &known[parent]).clone(),
                Place::Parked(_) => {
                    let parked = self.trees.local().get(&id).cloned();
                    if let Some(mut node) =
                        parked.filter(|node| node.shape.content != entry.content)
                    {
                        node.shape.content = entry.content.clone();
                        self.trees.set_local_changed(id, node);
                    }
                    continue;
                }
            };
            let node = LocalNode {
                left_out_when_placed: false,
                content_from_server: false,
                shape: Shape {
                    parent,
                    name: entry.name.clone(),
                    kind,
                    content: entry.content.clone(),
                },
                identity: identity(ino),
                parked: false,
            };
            self.trees.set_local_changed(id, node);
        }
        let on_disk: BTreeSet<&NodeId> = nodes.values().chain(&left_out).collect();
        let missing: BTreeSet<NodeId> = (reached.into_iter())
            .filter(|id| !on_disk.contains(id))
            .collect();
        let reported: BTreeSet<NodeId> = nodes.into_values().chain(placed).collect();
        self.displace(&reported);
        let gone = self.gone(&missing);
        for id in missing.iter().filter(|id| gone.contains(id)) {
            self.trees.remove_local(id);
        }
        self.drop_lost_folders();
        Reached {
            places,
            widened,
            observed,
        }
    }

    fn leave_out(
        &mut self,
        disk: &Disk,
        (ino, id): (u128, &NodeId),
        nodes: &BTreeMap<u128, NodeId>,
        known: &BTreeMap<u128, NodeId>,
    ) -> bool {
        let entry = &disk.entries()[&ino];
        let held = self.trees.local().get(id).expect("known in L").clone();
        let (parent, folder_left_out) = match &entry.place {
            Place::Root => (Some(self.trees.local().root.clone()), false),
            Place::In(parent) => (
                nodes.get(parent).or_else(|| known.get(parent)).cloned(),
                (disk.path(*parent)).is_some_and(|path| self.trees.ignores(&path, Kind::Dir)),
            ),
            Place::Parked(_) => return false,
        };
        let stays = parent.as_ref() == Some(&held.shape.parent) && entry.name == held.shape.name;
        if stays {
            if held.parked {
                let mut node = held;
                node.parked = false;
                self.trees.set_local_changed(id.clone(), node);
            }
            return false;
        }
        match (folder_left_out, parent) {
            (true, _) | (false, None) => {
                if !self.trees.recorded_local(id) {
                    self.trees.ignore_local(id);
                }
                false
            }
            (false, Some(parent)) => {
                let mut node = held;
                node.parked = false;
                node.shape.parent = parent;
                node.shape.name = entry.name.clone();
                self.trees.set_local_changed(id.clone(), node);
                true
            }
        }
    }

    fn left_out_in_l(&self, id: &NodeId) -> bool {
        let local = self.trees.local();
        let Some(node) = local.get(id) else {
            return false;
        };
        let mut path = Vec::new();
        let mut current = id;
        while *current != local.root {
            let Some(above) = local.get(current) else {
                return false;
            };
            path.push(above.shape.name.clone());
            current = &above.shape.parent;
        }
        path.reverse();
        self.trees.ignores(&path, node.shape.kind)
    }

    fn displace(&mut self, reported: &BTreeSet<NodeId>) {
        let local = self.trees.local();
        let taken = |id: &NodeId, node: &LocalNode| {
            (local.iter()).any(|(other, held)| {
                other != id
                    && reported.contains(other)
                    && !held.parked
                    && held.shape.parent == node.shape.parent
                    && name_key(&held.shape.name) == name_key(&node.shape.name)
            })
        };
        let holders: Vec<NodeId> = (local.iter())
            .filter(|(id, node)| {
                !node.parked
                    && !reported.contains(*id)
                    && (self.trees.ignored_local().contains(*id) || self.left_out_in_l(id))
                    && taken(id, node)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for holder in holders {
            let mut doomed = vec![holder];
            let mut next = 0;
            while let Some(current) = doomed.get(next).cloned() {
                doomed.extend(self.trees.local_children(&current).cloned());
                next += 1;
            }
            let recorded: Vec<bool> = (doomed.iter())
                .map(|id| self.trees.recorded_local(id))
                .collect();
            for (id, recorded) in doomed.iter().zip(recorded).rev() {
                match recorded {
                    true => self.trees.displace_local(id),
                    false => self.trees.remove_local(id),
                }
            }
        }
    }

    fn gone(&mut self, missing: &BTreeSet<NodeId>) -> Vec<NodeId> {
        let mut gone: Vec<NodeId> = Vec::new();
        loop {
            let parent_gone = |id: &NodeId| {
                (self.trees.local().get(id)).is_some_and(|node| gone.contains(&node.shape.parent))
            };
            let more: Vec<NodeId> = (missing.iter())
                .filter(|id| !gone.contains(*id) && self.trees.local().get(id).is_some())
                .filter(|id| !self.trees.recorded_local(id))
                .filter(|id| !self.left_out_in_l(id) || parent_gone(id))
                .cloned()
                .collect();
            if more.is_empty() {
                break;
            }
            gone.extend(more);
        }
        let recorded: Vec<NodeId> = self.trees.ignored_local().iter().cloned().collect();
        for node in recorded {
            let mut above = self
                .trees
                .local()
                .get(&node)
                .map(|n| n.shape.parent.clone());
            while let Some(folder) = above.filter(|folder| gone.contains(folder)) {
                gone.retain(|id| *id != folder);
                self.trees.keep_lost_local(&folder);
                above = self
                    .trees
                    .local()
                    .get(&folder)
                    .map(|n| n.shape.parent.clone());
            }
        }
        gone
    }

    fn drop_lost_folders(&mut self) {
        while let Some(folder) = (self.trees.lost_local().iter())
            .find(|folder| self.trees.local_children(folder).next().is_none())
            .cloned()
        {
            self.trees.remove_local(&folder);
        }
    }

    fn reach(&self, disk: &Disk, scope: &[String]) -> Reach {
        let local = self.trees.local();
        let whole = || Reach {
            places: vec![Vec::new()],
            widened: !scope.is_empty(),
            entries: disk.entries().keys().copied().collect(),
            nodes: local.iter().map(|(id, _)| id.clone()).collect(),
        };
        let entry_at = |path: &[String]| {
            path.iter().try_fold(None, |parent, name| {
                let place = parent.map_or(Place::Root, Place::In);
                disk.child(&place, name).map(Some)
            })?
        };
        let held_in = |parent: NodeId| {
            (local.iter()).filter(move |(_, node)| !node.parked && node.shape.parent == parent)
        };
        let node_at = |path: &[String]| {
            path.iter().try_fold(&local.root, |parent, name| {
                let child = held_in(parent.clone()).find(|(_, node)| node.shape.name == *name);
                child.map(|(id, _)| id)
            })
        };
        let path_of = |id: &NodeId| {
            let mut names = Vec::new();
            let mut current = id;
            while *current != local.root {
                let node = local.get(current).filter(|node| !node.parked)?;
                names.push(node.shape.name.clone());
                current = &node.shape.parent;
            }
            names.reverse();
            Some(names)
        };
        let holds = |path: &[String]| {
            path.is_empty()
                || (node_at(path).and_then(|id| local.get(id))).is_some_and(|node| {
                    node.shape.kind == Kind::Dir && Some(node.identity.ino) == entry_at(path)
                })
        };
        let below_on_disk = |top: u128| {
            (disk.entries().iter())
                .filter(move |(ino, entry)| **ino == top || disk.inside(&entry.place, top))
                .map(|(ino, _)| *ino)
        };
        let below_in_l = |top: &NodeId| {
            (local.iter())
                .filter(|(id, _)| {
                    std::iter::successors(Some(*id), |id| {
                        local.get(id).map(|node| &node.shape.parent)
                    })
                    .take(local.len() + 1)
                    .any(|above| above == top)
                })
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>()
        };
        let known: BTreeMap<u128, &NodeId> = (local.iter())
            .map(|(id, node)| (node.identity.ino, id))
            .collect();
        let mut reach = Reach::default();
        let mut pending = vec![scope.to_vec()];
        while let Some(asked) = pending.pop() {
            let mut place = asked.clone();
            if place.is_empty() {
                return whole();
            }
            while !holds(&place[..place.len() - 1]) {
                place.pop();
            }
            reach.widened |= place != asked;
            if reach.places.contains(&place) {
                continue;
            }
            let (parent, focus) = place.split_at(place.len() - 1);
            let listed = match entry_at(parent) {
                Some(ino) => Place::In(ino),
                None => Place::Root,
            };
            let parent = node_at(parent).expect("a folder L holds");
            let names: BTreeSet<&String> = (disk.children(&listed))
                .map(|ino| &disk.entries()[&ino].name)
                .collect();
            for ino in disk.children(&listed) {
                let name = &disk.entries()[&ino].name;
                if *name == focus[0]
                    || held_in(parent.clone()).all(|(_, node)| node.shape.name != *name)
                {
                    reach.entries.extend(below_on_disk(ino));
                }
            }
            for (id, node) in held_in(parent.clone()) {
                if node.shape.name == focus[0] || !names.contains(&node.shape.name) {
                    reach.nodes.extend(below_in_l(id));
                }
            }
            reach.places.push(place);
            for id in &reach.nodes {
                let ino = local.get(id).expect("reached in L").identity.ino;
                if disk.entries().contains_key(&ino) && !reach.entries.contains(&ino) {
                    match disk.path(ino) {
                        Some(path) => pending.push(path),
                        None => return whole(),
                    }
                }
            }
            for ino in &reach.entries {
                if let Some(id) = known.get(ino).filter(|id| !reach.nodes.contains(**id)) {
                    match path_of(id) {
                        Some(path) => pending.push(path),
                        None => return whole(),
                    }
                }
            }
        }
        reach
    }
}

pub struct Reached {
    pub places: Vec<Vec<String>>,
    pub widened: bool,
    pub observed: BTreeSet<u128>,
}

#[derive(Default)]
struct Reach {
    places: Vec<Vec<String>>,
    widened: bool,
    entries: BTreeSet<u128>,
    nodes: BTreeSet<NodeId>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rules {
    pub exclude: Vec<&'static str>,
    pub include: Vec<&'static str>,
    pub file: Vec<&'static str>,
    pub changed: bool,
}

impl Rules {
    pub fn read(&self, trees: &mut Trees) {
        if self.changed {
            let root: [&str; 0] = [];
            let file =
                (!self.file.is_empty()).then(|| RuleFile::parse(self.file.join("\n").as_bytes()));
            trees.set_rules(&root, Source::Koiignore, file);
        }
        if self.changed || !self.exclude.is_empty() {
            let local = LocalRules::new(&self.include, &self.exclude).expect("valid local rules");
            trees.set_local_rules(local);
        }
        trees.rules_scanned();
    }

    pub fn leave_out(&self) -> bool {
        !self.exclude.is_empty() || !self.include.is_empty() || !self.file.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Ruling {
    File(Vec<&'static str>),
    Exclude(Vec<&'static str>),
    Include(Vec<&'static str>),
}

/// A message on its way from the server to a device.
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    /// Commits of other devices, or the backlog after a `Resume`.
    Records(Vec<CommitRecord>),
    /// The answer to the commit sent as `request`.
    Answer {
        request: u64,
        result: Result<CommitResult, Error>,
    },
    /// The server's tree and trash at `head`, with the commit token there, listed for a device
    /// whose R the server is behind.
    Listing {
        nodes: Vec<NodeState>,
        head: Base,
        history: History,
    },
}

/// A connection. Breaking it loses every message still on its way.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Link {
    pub inbox: Vec<Message>,
    /// The server's head when the session began. The device is caught up once `last_seq` reaches it.
    pub head: u64,
    /// The request whose answer the device waits for.
    pub awaiting: Option<u64>,
    pub requests: u64,
    pub listing: bool,
}

pub struct Device {
    pub session: Session,
    pub disk: Disk,
    pub rules: Rules,
    pub engine: Engine,
    /// The engine state as the last store transaction left it.
    pub store: Engine,
    /// Crashed and not restarted yet.
    pub down: bool,
    pub link: Option<Link>,
    /// Why the device last started a recovery (section 6.4).
    pub recovery: Option<String>,
    pub recovery_from: Resume,
    pub store_copy: Option<Engine>,
    pub sent: BTreeMap<u64, BTreeSet<String>>,
    pub sent_copy: BTreeMap<u64, BTreeSet<String>>,
    pub rolled_back: bool,
    pub restored_ahead: bool,
    pub left: bool,
}

impl Device {
    pub fn read_rules_file(&mut self, texts: &BTreeMap<Vec<u8>, Vec<&'static str>>) -> bool {
        let file = (self.disk.child(&Place::Root, RULES_FILE))
            .and_then(|ino| self.disk.entries()[&ino].content.as_ref())
            .and_then(|content| texts.get(&content.manifest_digest).cloned())
            .unwrap_or_default();
        if file == self.rules.file {
            return false;
        }
        self.rules.file = file;
        self.rules.changed = true;
        self.rules.read(&mut self.engine.trees);
        true
    }

    /// Whether the device started a recovery and has not classified the trees for it yet.
    pub fn recovering(&self) -> bool {
        let engine = if self.down { &self.store } else { &self.engine };
        matches!(engine.commits.halted(), Some(Halt::Recovery(_)))
    }

    /// The end of one call into the engine: what needs no decision is settled, and the state goes
    /// to the store in one transaction.
    pub fn persist(&mut self) -> Result<(), Failure> {
        self.engine.trees.settle(self.engine.commits.aliases());
        let (engine, store) = (&mut self.engine, &self.store);
        let differences: Vec<String> = [
            differences(
                "trees",
                &engine.trees.take_changes(),
                &TreesChanges::between(&store.trees.state(), &engine.trees.state()),
            ),
            differences(
                "commits",
                &engine.commits.take_changes(),
                &CommitsChanges::between(&store.commits.state(), &engine.commits.state()),
            ),
            differences(
                "effects",
                &engine.effects.take_changes(),
                &EffectsChanges::between(&store.effects.state(), &engine.effects.state()),
            ),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !differences.is_empty() {
            return Err(Failure::Broken(format!(
                "the tracked changes differ from the state diff: {}",
                differences.join("; ")
            )));
        }
        self.store = self.engine.reload(&self.rules)?;
        let commits = &self.store.commits;
        if let Some(flight) = commits.in_flight() {
            (self.sent.entry(flight.commit.device_seq).or_default())
                .insert(flight.commit.commit_id.clone());
        }
        if let Some(&oldest) = commits.slots().keys().next() {
            self.sent = self.sent.split_off(&oldest);
        }
        for (slot, ids) in &self.sent {
            let given_up = commits.given_up().get(slot);
            if given_up.is_some_and(|ids| ids.len() >= GIVEN_UP_KEPT) {
                continue;
            }
            let known = |id: &String| {
                (commits.slots().get(slot)).is_some_and(|kept| kept.commit_id == *id)
                    || given_up.is_some_and(|ids| ids.contains(id))
                    || (commits.in_flight()).is_some_and(|flight| flight.commit.commit_id == *id)
            };
            if let Some(id) = ids.iter().find(|id| !known(id)) {
                return Err(Failure::Broken(format!(
                    "the store forgot commit {id}, which it sent at slot {slot}: it is neither the slot's result nor given up"
                )));
            }
        }
        Ok(())
    }

    /// A full rescan. After it the engine plans the nodes again that the server refused an op
    /// for (section 2.7). Returns how many there were.
    pub fn rescan(&mut self) -> usize {
        self.engine.scan(&self.disk);
        let problems: Vec<NodeId> = self.engine.commits.problems().iter().cloned().collect();
        for node in &problems {
            self.engine.commits.clear_problem(node);
        }
        problems.len()
    }
}

fn differences<T: PartialEq + Debug>(part: &str, tracked: &T, diff: &T) -> Option<String> {
    if tracked == diff {
        return None;
    }
    let fields = |changes: &T| {
        let mut fields: Vec<(String, String)> = Vec::new();
        for line in format!("{changes:#?}").lines() {
            let Some(line) = line.strip_prefix("    ") else {
                continue;
            };
            match line.split_once(": ").filter(|_| !line.starts_with(' ')) {
                Some((name, value)) => fields.push((name.to_owned(), value.to_owned())),
                None => {
                    if let Some((_, value)) = fields.last_mut() {
                        value.push_str(line.trim_start());
                    }
                }
            }
        }
        fields
    };
    let fields: Vec<String> = (fields(tracked).into_iter())
        .zip(fields(diff))
        .filter(|(tracked, diff)| tracked != diff)
        .map(|((name, tracked), (_, diff))| format!("{part}.{name}: tracked {tracked} diff {diff}"))
        .collect();
    Some(match fields.is_empty() {
        true => format!("{part}: tracked {tracked:?} diff {diff:?}"),
        false => fields.join("; "),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unequal_changes_differ_whatever_their_debug_text() {
        let shape = |name: &str| Shape {
            parent: "root".into(),
            name: name.into(),
            kind: Kind::Dir,
            content: None,
        };
        let tracked = TreesChanges {
            discarded: vec![("node".into(), Some(shape("a,)")))],
            ..TreesChanges::default()
        };
        let diff = TreesChanges {
            discarded: vec![("node".into(), Some(shape("a)")))],
            ..TreesChanges::default()
        };
        let message = differences("trees", &tracked, &diff).expect("unequal changes differ");
        assert!(message.starts_with("trees.discarded: "), "{message}");
        assert!(message.contains(r#"name: "a,)""#), "{message}");
        assert!(message.contains(r#"name: "a)""#), "{message}");
        assert_eq!(differences("trees", &tracked, &tracked), None);
    }
}
