// SPDX-License-Identifier: Apache-2.0

//! One device of the simulation: its disk, the engine state, the copy of that state the store
//! holds, and the connection to the server.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Debug;

use koidrive_engine::classify::Trees;
use koidrive_engine::commit::{Base, Commits, Halt};
use koidrive_engine::effects::Effects;
use koidrive_engine::ids::IdGen;
use koidrive_engine::state::{CommitsChanges, EffectsChanges, TreesChanges};
use koidrive_engine::tree::{Kind, LocalNode, NodeId, RemoteNode, Shape};
use koidrive_proto::v1::{
    ChangeRecord, CommitRecord, CommitResult, Error, NodeState, change_record,
};

use super::disk::{Disk, Place, identity};
use super::{Failure, NOW};
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
    /// comes back equal. The store does not hold the ignore rules. These devices have no rules of
    /// their own, and a restart plans nothing before its full scan, so the rules count as read.
    pub fn reload(&self) -> Result<Engine, Failure> {
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
        trees.rules_scanned();
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
        let root = self.trees.local().root.clone();
        let known: BTreeMap<u128, NodeId> = (self.trees.local().iter())
            .map(|(id, node)| (node.identity.ino, id.clone()))
            .collect();
        let mut nodes: BTreeMap<u128, NodeId> = BTreeMap::new();
        let mut queue: VecDeque<u128> = (disk.entries().iter())
            .filter(|(_, entry)| !matches!(entry.place, Place::In(_)))
            .map(|(ino, _)| *ino)
            .collect();
        while let Some(ino) = queue.pop_front() {
            let entry = &disk.entries()[&ino];
            let id = match known.get(&ino) {
                Some(id) => id.clone(),
                None => self.ids.uuid_v7(NOW as u64),
            };
            nodes.insert(ino, id.clone());
            queue.extend(disk.children(&Place::In(ino)));
            let parent = match &entry.place {
                Place::Root => root.clone(),
                Place::In(parent) => nodes[parent].clone(),
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
                shape: Shape {
                    parent,
                    name: entry.name.clone(),
                    kind: match entry.content {
                        Some(_) => Kind::File,
                        None => Kind::Dir,
                    },
                    content: entry.content.clone(),
                },
                identity: identity(ino),
                parked: false,
            };
            if self.trees.local().get(&id) != Some(&node) {
                self.trees.set_local_changed(id, node);
            }
        }
        let on_disk: BTreeSet<&NodeId> = nodes.values().collect();
        let gone: Vec<NodeId> = (self.trees.local().iter())
            .map(|(id, _)| id)
            .filter(|id| !on_disk.contains(id))
            .cloned()
            .collect();
        for id in gone {
            self.trees.remove_local(&id);
        }
    }
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
    Listing { nodes: Vec<NodeState>, head: Base },
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
    pub engine: Engine,
    /// The engine state as the last store transaction left it.
    pub store: Engine,
    /// Crashed and not restarted yet.
    pub down: bool,
    pub link: Option<Link>,
    /// Why the device last started a recovery (section 6.4).
    pub recovery: Option<String>,
    pub store_copy: Option<Engine>,
    pub rolled_back: bool,
    pub left: bool,
}

impl Device {
    /// Whether the device started a recovery and has not classified the trees for it yet.
    pub fn recovering(&self) -> bool {
        let engine = if self.down { &self.store } else { &self.engine };
        matches!(engine.commits.halted(), Some(Halt::Recovery(_)))
    }

    /// The end of one call into the engine: what needs no decision is settled, and the state goes
    /// to the store in one transaction.
    pub fn persist(&mut self) -> Result<(), Failure> {
        self.engine.trees.settle();
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
        self.store = self.engine.reload()?;
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
