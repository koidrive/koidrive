// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet, HashMap};

use koidrive_engine::classify::{Class, Rescope, Trees};
use koidrive_engine::effects::{self, Stuck};
use koidrive_engine::tree::{Content, Kind, NodeId, Shape, same_content};

use crate::engine::Engine;
use crate::progress::{Direction, Item, Tracker};
use crate::status::FolderState;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    Out,
    Held,
    Waiting,
    Other,
    Moves,
}

/// Where each dirty node stands, kept from what each step changed.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Outstanding {
    rows: HashMap<NodeId, Row>,
    unsettled: u64,
    other: u64,
    waiting: BTreeSet<NodeId>,
    asked: BTreeSet<NodeId>,
    held: BTreeMap<NodeId, Stuck>,
    blocking: HashMap<NodeId, Vec<NodeId>>,
    above_blocking: HashMap<NodeId, u32>,
    shown: bool,
    unshown: BTreeSet<NodeId>,
}

impl Outstanding {
    pub fn refresh(&mut self, engine: &Engine) {
        let mut touched = engine.touched.take();
        let rescope = engine.trees.take_rescope();
        if !touched.stepped && rescope == Rescope::default() {
            return;
        }
        touched.rescope.add(rescope);
        if touched.rescope.all {
            *self = Outstanding::build(engine);
            return;
        }
        let mut nodes = touched.nodes;
        nodes.extend(touched.rescope.nodes.iter().cloned());
        let held = held(engine);
        nodes.extend(changed_keys(&self.held, &held));
        self.held = held;
        if touched.questions {
            let asked = asked(engine);
            nodes.extend(self.asked.symmetric_difference(&asked).cloned());
            self.asked = asked;
        }
        nodes.extend(reach(&engine.trees, &touched.rescope));
        let mut affected = BTreeSet::new();
        for id in &nodes {
            self.chain(engine, id, &mut affected);
        }
        nodes.extend(affected);
        nodes.extend(touched.rescope.left.iter().cloned());
        nodes.extend(ancestors(&engine.trees, &nodes));
        let kept = engine.trees.kept_for_excluded();
        for id in nodes {
            let row = self.row(engine, &kept, &id);
            self.set(id, row);
        }
    }

    fn build(engine: &Engine) -> Outstanding {
        let mut built = Outstanding {
            held: held(engine),
            asked: asked(engine),
            ..Outstanding::default()
        };
        let blocking: Vec<NodeId> = (engine.effects.blocked().keys())
            .chain(built.held.keys())
            .cloned()
            .collect();
        for id in blocking {
            built.chain(engine, &id, &mut BTreeSet::new());
        }
        let kept = engine.trees.kept_for_excluded();
        for id in engine.trees.dirty() {
            let row = built.row(engine, &kept, id);
            built.set(id.clone(), row);
        }
        built
    }

    fn chain(&mut self, engine: &Engine, id: &NodeId, affected: &mut BTreeSet<NodeId>) {
        let blocks = engine.effects.blocked().contains_key(id) || self.held.contains_key(id);
        let old = match blocks {
            true => self
                .blocking
                .insert(id.clone(), local_ancestors(&engine.trees, id)),
            false => self.blocking.remove(id),
        };
        for ancestor in old.into_iter().flatten() {
            let count = self.above_blocking.get_mut(&ancestor).expect("counted");
            *count -= 1;
            if *count == 0 {
                self.above_blocking.remove(&ancestor);
                affected.insert(ancestor);
            }
        }
        for ancestor in self.blocking.get(id).into_iter().flatten() {
            let count = self.above_blocking.entry(ancestor.clone()).or_default();
            *count += 1;
            if *count == 1 {
                affected.insert(ancestor.clone());
            }
        }
    }

    fn row(&self, engine: &Engine, kept: &impl Fn(&str) -> bool, id: &NodeId) -> Option<Row> {
        let trees = &engine.trees;
        if !trees.dirty().contains(id) {
            return None;
        }
        let out = engine.effects.blocked().contains_key(id)
            || engine.commits.problems().contains(id)
            || trees.excluded(id)
            || kept(id)
            || (self.above_blocking.contains_key(id) && trees.remote().live(id).is_none())
            || self.held.contains_key(id);
        let asked = self.asked.contains(id);
        Some(match () {
            _ if out => Row::Out,
            _ if trees.held(id) && asked => Row::Waiting,
            _ if trees.held(id) => Row::Held,
            _ if wants(trees, id).next().is_some() => Row::Moves,
            _ if asked => Row::Waiting,
            _ => Row::Other,
        })
    }

    fn set(&mut self, id: NodeId, row: Option<Row>) {
        let old = match row {
            Some(row) => self.rows.insert(id.clone(), row),
            None => self.rows.remove(&id),
        };
        if old == row {
            if row == Some(Row::Moves) && self.shown {
                self.unshown.insert(id);
            }
            return;
        }
        let count = |row: Option<Row>, of: fn(Row) -> bool| u64::from(row.is_some_and(of));
        self.unsettled =
            self.unsettled + count(row, |row| row != Row::Out) - count(old, |row| row != Row::Out);
        self.other =
            self.other + count(row, |row| row == Row::Other) - count(old, |row| row == Row::Other);
        match row == Some(Row::Waiting) {
            true => self.waiting.insert(id.clone()),
            false => self.waiting.remove(&id),
        };
        if self.shown && (old == Some(Row::Moves) || row == Some(Row::Moves)) {
            self.unshown.insert(id);
        }
    }

    /// True when no dirty node can move: each is one that cannot be synced as it is, that this device
    /// leaves out, a folder the user deleted that stays on the server for what this device leaves out, a folder to
    /// remove that L holds above a node that cannot be synced, or a node of [`Outstanding::held`].
    pub fn settled(&self) -> bool {
        self.unsettled == 0
    }

    /// The nodes this device's rules keep from their place or from S ([`effects::stuck`]).
    pub fn held(&self) -> &BTreeMap<NodeId, Stuck> {
        &self.held
    }

    pub fn other_changes(&self) -> u64 {
        self.other
    }

    pub fn questions_holding_changes(&self, engine: &Engine) -> u64 {
        (engine.commits.questions().iter())
            .filter(|question| question.holds().any(|id| self.waiting.contains(id)))
            .count() as u64
    }

    /// Gives the tracker the files to upload and download, and says whether the progress began or ended.
    pub fn show(&mut self, state: FolderState, engine: &Engine, tracker: &mut Tracker) -> bool {
        match state {
            FolderState::Bootstrapping | FolderState::CatchingUp => {}
            FolderState::Connecting | FolderState::Offline => return false,
            FolderState::Stopped | FolderState::UpToDate | FolderState::Halted => {
                self.shown = false;
                self.unshown.clear();
                return tracker.clear();
            }
        }
        let trees = &engine.trees;
        let synced = |item: &Item| {
            let holds = match item.direction {
                Direction::Upload => file(trees.synced().live(&item.node)),
                Direction::Download => file(trees.local().live(&item.node)),
            };
            holds.is_some_and(|content| content.manifest_digest == item.digest)
        };
        if !self.shown {
            self.shown = true;
            self.unshown.clear();
            let wanted = (self.rows.iter())
                .filter(|(_, row)| **row == Row::Moves)
                .flat_map(|(id, _)| items(trees, id))
                .collect();
            return tracker.track(wanted, synced);
        }
        let nodes = std::mem::take(&mut self.unshown).into_iter().map(|id| {
            let wanted = match self.rows.get(&id) {
                Some(Row::Moves) => items(trees, &id).collect(),
                _ => Vec::new(),
            };
            (id, wanted)
        });
        tracker.update(nodes, synced)
    }

    /// Panics unless every count and row is what a recount from scratch finds.
    #[cfg(test)]
    pub fn check(&self, engine: &Engine) {
        let mut built = Outstanding::build(engine);
        built.shown = self.shown;
        built.unshown = self.unshown.clone();
        let rows: BTreeMap<_, _> = (self.rows.keys().chain(built.rows.keys()))
            .filter(|id| self.rows.get(*id) != built.rows.get(*id))
            .map(|id| (id, (self.rows.get(id), built.rows.get(id))))
            .collect();
        assert_eq!(rows, BTreeMap::new(), "rows as kept and as recounted");
        assert_eq!(
            *self, built,
            "the outstanding nodes as kept and as recounted"
        );
    }
}

fn held(engine: &Engine) -> BTreeMap<NodeId, Stuck> {
    match (engine.effects.in_flight(), engine.commits.in_flight()) {
        ([], None) => effects::stuck(&engine.trees, &engine.effects, None),
        _ => BTreeMap::new(),
    }
}

fn asked(engine: &Engine) -> BTreeSet<NodeId> {
    (engine.commits.questions().iter())
        .flat_map(|question| question.holds())
        .cloned()
        .collect()
}

fn changed_keys<'a, V: PartialEq>(
    before: &'a BTreeMap<NodeId, V>,
    after: &'a BTreeMap<NodeId, V>,
) -> impl Iterator<Item = NodeId> + 'a {
    let gone = (before.keys()).filter(|id| !after.contains_key(*id));
    let came = (after.keys()).filter(|id| !before.contains_key(*id));
    gone.chain(came).cloned()
}

fn file(shape: Option<&Shape>) -> Option<&Content> {
    (shape.filter(|shape| shape.kind == Kind::File)).and_then(|shape| shape.content.as_ref())
}

fn wants<'a>(trees: &'a Trees, id: &'a NodeId) -> impl Iterator<Item = (Direction, &'a Content)> {
    let other = |a: &Content, b: Option<&Content>| b.is_none_or(|b| !same_content(a, b));
    let local = file(trees.local().live(id));
    let synced = file(trees.synced().live(id));
    let remote = file(trees.remote().live(id));
    let class = trees.class(id);
    let upload = local.filter(|local| {
        matches!(class, Class::Local | Class::Concurrent)
            && other(local, remote)
            && (other(local, synced) || trees.unsent_edits().contains(id))
    });
    let download = remote.filter(|remote| {
        matches!(class, Class::Remote | Class::Concurrent)
            && other(remote, synced)
            && other(remote, local)
    });
    (upload.map(|content| (Direction::Upload, content)))
        .into_iter()
        .chain(download.map(|content| (Direction::Download, content)))
}

fn items<'a>(trees: &'a Trees, id: &'a NodeId) -> impl Iterator<Item = (Item, u64)> + 'a {
    wants(trees, id).map(move |(direction, content)| {
        let item = Item {
            direction,
            node: id.clone(),
            digest: content.manifest_digest.clone(),
        };
        (item, content.size)
    })
}

fn local_ancestors(trees: &Trees, id: &str) -> Vec<NodeId> {
    let mut above = Vec::new();
    let mut current = id;
    while let Some(local) = trees.local().get(current)
        && local.shape.parent != trees.local().root
        && above.len() < trees.local().len()
    {
        current = &local.shape.parent;
        above.push(current.to_owned());
    }
    above
}

/// The folders above `nodes` in L and in R, whose exclusion and rows read what is below them.
fn ancestors(trees: &Trees, nodes: &BTreeSet<NodeId>) -> BTreeSet<NodeId> {
    fn parent<'a>(trees: &'a Trees, remote: bool, id: &str) -> Option<&'a NodeId> {
        match remote {
            false => Some(&trees.local().get(id)?.shape.parent),
            true => Some(&trees.remote().live(id)?.parent),
        }
    }
    let mut found = BTreeSet::new();
    for id in nodes {
        for remote in [false, true] {
            let mut current = id.as_str();
            while let Some(above) = parent(trees, remote, current)
                && found.insert(above.clone())
            {
                current = above;
            }
        }
    }
    found
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Side {
    Local,
    Synced,
    Remote,
}

/// The nodes `rescope` names below its folders, each with what is below it on the same side and in R.
fn reach(trees: &Trees, rescope: &Rescope) -> BTreeSet<NodeId> {
    let mut seen: BTreeSet<(Side, NodeId)> = BTreeSet::new();
    let mut work: Vec<(Side, NodeId)> = (rescope.local.iter().map(|id| (Side::Local, id.clone())))
        .chain(rescope.synced.iter().map(|id| (Side::Synced, id.clone())))
        .chain(rescope.remote.iter().map(|id| (Side::Remote, id.clone())))
        .collect();
    while let Some((side, id)) = work.pop() {
        if !seen.insert((side, id.clone())) {
            continue;
        }
        let same: Vec<&NodeId> = match side {
            Side::Local => trees.local_children_placed_or_parked(&id).collect(),
            Side::Synced => trees.synced_children(&id).collect(),
            Side::Remote => Vec::new(),
        };
        let below = (same.into_iter().map(|child| (side, child.clone()))).chain(
            trees
                .remote_children(&id)
                .map(|child| (Side::Remote, child.clone())),
        );
        work.extend(below.filter(|next| !seen.contains(next)));
    }
    seen.into_iter().map(|(_, id)| id).collect()
}
