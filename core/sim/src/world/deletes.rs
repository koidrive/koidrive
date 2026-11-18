// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use koidrive_engine::tree::{Content, NodeId, RemoteNode};

use koidrive_proto::v1::NodeStatus;

use super::{Failure, SpaceDo, Spent, World};

pub(super) fn live_nodes(space: &SpaceDo) -> BTreeMap<i64, BTreeSet<NodeId>> {
    let mut live: BTreeMap<i64, BTreeSet<NodeId>> = BTreeMap::new();
    for node in space.nodes() {
        if node.status != NodeStatus::Live as i32 {
            continue;
        }
        if let Some(content) = node.current.and_then(|version| version.content) {
            live.entry(content.mtime_ns)
                .or_default()
                .insert(node.node_id);
        }
    }
    live
}

pub(super) struct Deleted {
    node: NodeId,
    write: i64,
    before: BTreeSet<i64>,
    path: Vec<NodeId>,
    elsewhere: BTreeSet<NodeId>,
}

pub(super) struct Deletion {
    pub(super) device: usize,
    at: u64,
    files: Vec<Deleted>,
    unsent: BTreeSet<(usize, u128, u64)>,
    resendable: BTreeSet<i64>,
    pub(super) reached: bool,
    pub(super) gone: bool,
    pub(super) carried_at: Option<u64>,
}

impl Deletion {
    pub(super) fn carried(&mut self, seq: u64, deleted: &BTreeSet<&str>) {
        if (self.files.iter())
            .any(|file| (file.path.last()).is_some_and(|root| deleted.contains(root.as_str())))
        {
            self.carried_at.get_or_insert(seq);
        }
    }

    pub(super) fn held(
        &self,
        live: &BTreeMap<i64, BTreeSet<NodeId>>,
        skip: impl Fn(i64, &NodeId, &Deleted, bool) -> bool,
    ) -> Option<i64> {
        (self.files.iter()).find_map(|file| {
            let nodes = |write: &i64| live.get(write).into_iter().flatten();
            let at_node = std::iter::once(&file.write)
                .chain(&file.before)
                .filter(|write| nodes(write).any(|there| *there == file.node))
                .map(|write| (*write, &file.node, true));
            let elsewhere = nodes(&file.write)
                .filter(|there| **there != file.node && !file.elsewhere.contains(*there))
                .map(|there| (file.write, there, false));
            at_node
                .chain(elsewhere)
                .find(|(write, there, at_node)| !skip(*write, there, file, *at_node))
                .map(|(write, _, _)| write)
        })
    }
}

impl World {
    pub(super) fn record_delete(
        &mut self,
        d: usize,
        ino: u128,
        gone: &BTreeSet<u128>,
        deleted: &[Content],
    ) {
        let device = &self.devices[d];
        let trees = &device.store.trees;
        let synced = trees.synced();
        let target = (trees.local().iter())
            .find(|(_, node)| node.identity.ino == ino)
            .map(|(target, _)| target);
        let above = |id: &NodeId| {
            std::iter::successors(Some(id.clone()), |id| {
                synced.get(id).map(|shape| shape.parent.clone())
            })
            .collect::<Vec<_>>()
        };
        let under_target = |id: &NodeId| target.is_some_and(|target| above(id).contains(target));
        let live = live_nodes(&self.space);
        let mut taken = BTreeSet::new();
        let files: Vec<Deleted> = (deleted.iter())
            .filter_map(|content| {
                let write = content.mtime_ns;
                let before: BTreeSet<i64> =
                    std::iter::successors(self.overwrote.get(&write), |write| {
                        self.overwrote.get(write)
                    })
                    .copied()
                    .collect();
                let holders: Vec<&NodeId> = (synced.iter())
                    .filter(|(id, shape)| {
                        !taken.contains(*id)
                            && (shape.content.as_ref()).is_some_and(|c| {
                                c.mtime_ns == write || before.contains(&c.mtime_ns)
                            })
                    })
                    .map(|(id, _)| id)
                    .collect();
                let node = (holders.iter())
                    .find(|id| under_target(id))
                    .or(holders.last())
                    .copied()?;
                taken.insert(node.clone());
                Some(Deleted {
                    node: node.clone(),
                    write,
                    before,
                    path: std::iter::successors(Some(node.clone()), |id| {
                        let parent = &trees.local().get(id)?.shape.parent;
                        (trees.local().get(parent))
                            .is_some_and(|node| gone.contains(&node.identity.ino))
                            .then(|| parent.clone())
                    })
                    .collect(),
                    elsewhere: (live.get(&write).into_iter().flatten())
                        .filter(|there| *there != node && !under_target(there))
                        .cloned()
                        .collect(),
                })
            })
            .collect();
        let server: BTreeMap<NodeId, RemoteNode> = (self.space.nodes().iter())
            .filter_map(|state| RemoteNode::from_state(state).ok())
            .collect();
        let scope: Vec<_> = (synced.iter())
            .filter(|(id, _)| under_target(id) || (files.iter()).any(|file| file.path.contains(id)))
            .collect();
        let (touched, touches) = (&self.touched_nodes, &self.touches);
        let unsent: BTreeSet<(usize, u128, u64)> = (0..self.devices.len())
            .filter(|other| *other != d)
            .flat_map(|other| {
                (scope.iter())
                    .filter_map(move |(id, _)| touched[other].get(*id))
                    .flat_map(move |ino| {
                        (touches[other].get(ino).into_iter().flatten())
                            .map(move |at| (other, *ino, *at))
                    })
            })
            .collect();
        let seen = target.is_some_and(|target| {
            synced.get(target).is_some()
                && (scope.iter()).all(|(id, shape)| {
                    (server.get(*id)).is_some_and(|there| !there.trashed && there.shape.same(shape))
                })
        });
        if seen
            && !device.rolled_back
            && !device.recovering()
            && device.engine.history == self.space.history_id()
        {
            let mut deletion = Deletion {
                device: d,
                at: self.clock,
                resendable: (files.iter())
                    .map(|file| file.write)
                    .filter(|write| self.exposed.contains_key(write) && !live.contains_key(write))
                    .collect(),
                files,
                unsent,
                reached: false,
                gone: false,
                carried_at: None,
            };
            deletion.reached = deletion.held(&live, |_, _, _, _| false).is_none();
            self.deletions.push(deletion);
        }
    }

    pub(super) fn forget_restored_deletes<'a>(
        &mut self,
        d: usize,
        restored: impl IntoIterator<Item = &'a NodeId>,
    ) {
        let restored: BTreeSet<&NodeId> = restored.into_iter().collect();
        self.deletions.retain_mut(|deletion| {
            if deletion.device == d {
                (deletion.files).retain(|file| !restored.contains(&file.node));
            }
            deletion.device != d || !deletion.files.is_empty()
        });
    }

    pub(super) fn revived_delete(&self) -> Result<(), Failure> {
        if !self.revived_deletes {
            return Ok(());
        }
        let live = live_nodes(&self.space);
        let dropped = (self.deletions.iter())
            .filter(|deletion| !deletion.reached && !deletion.gone)
            .find_map(|deletion| {
                let write = deletion.held(&live, |write, node, file, at_node| {
                    let changed_since = |spent: &Spent| {
                        let is = |id: &NodeId| {
                            self.touched_nodes[spent.device].get(id) == Some(&spent.ino)
                        };
                        let itself = is(node) || is(&file.node);
                        let moved_between = self.moves[spent.device].contains(&spent.at)
                            && (file.path.iter()).skip(1).any(is);
                        let revived = spent.revived.contains(node) && (file.path.iter()).any(is);
                        spent.device != deletion.device
                            && (self.actions[spent.device].get(&spent.at))
                                .is_some_and(|(own, _)| *own == spent.ino)
                            && ((deletion.unsent).contains(&(spent.device, spent.ino, spent.at))
                                || spent.at >= deletion.at)
                            && (itself || moved_between || revived)
                    };
                    deletion.resendable.contains(&write)
                        || (!at_node && self.copied.contains(&write))
                        || self.spent.iter().any(changed_since)
                })?;
                Some((write, deletion.device))
            });
        match dropped {
            None => Ok(()),
            Some((write, d)) => Err(Failure::Diverged(format!(
                "the user of device {d} deleted write {write} or a later write made over it, and \
                 the server held it live from then on: the delete never reached the server"
            ))),
        }
    }
}
