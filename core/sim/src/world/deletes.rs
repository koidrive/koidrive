// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use koidrive_engine::tree::{Content, NodeId, RemoteNode};

use super::{Failure, World, live_writes};

pub(super) struct Deletion {
    device: usize,
    at: u64,
    files: Vec<(NodeId, i64, BTreeSet<i64>)>,
    resendable: BTreeSet<i64>,
    pub(super) reached: bool,
    pub(super) gone: bool,
}

impl Deletion {
    pub(super) fn held(
        &self,
        live: &BTreeMap<i64, NodeId>,
        skip: impl Fn(i64) -> bool,
    ) -> Option<i64> {
        (self.files.iter()).find_map(|(node, write, before)| {
            std::iter::once(write)
                .filter(|write| live.contains_key(write))
                .chain(before.iter().filter(|write| live.get(write) == Some(node)))
                .copied()
                .find(|write| !skip(*write))
        })
    }
}

impl World {
    pub(super) fn record_delete(&mut self, d: usize, ino: u128, deleted: &[Content]) {
        let device = &self.devices[d];
        let trees = &device.store.trees;
        let synced = trees.synced();
        let synced_writes: BTreeMap<i64, &NodeId> = (synced.iter())
            .filter_map(|(id, shape)| Some((shape.content.as_ref()?.mtime_ns, id)))
            .collect();
        let files: Vec<(NodeId, i64, BTreeSet<i64>)> = (deleted.iter())
            .filter_map(|content| {
                let write = content.mtime_ns;
                let before = std::iter::successors(self.overwrote.get(&write), |write| {
                    self.overwrote.get(write)
                });
                Some((
                    (*synced_writes.get(&write)?).clone(),
                    write,
                    before.copied().collect(),
                ))
            })
            .collect();
        let live = live_writes(&self.space);
        let server: BTreeMap<NodeId, RemoteNode> = (self.space.nodes().iter())
            .filter_map(|state| RemoteNode::from_state(state).ok())
            .collect();
        let above = |id: &NodeId| {
            std::iter::successors(Some(id.clone()), |id| {
                synced.get(id).map(|shape| shape.parent.clone())
            })
            .collect::<Vec<_>>()
        };
        let pending = |id: &NodeId| {
            (0..self.devices.len())
                .filter(|other| *other != d)
                .any(|other| {
                    (self.touched_nodes[other].get(id))
                        .and_then(|ino| self.touches[other].get(ino))
                        .is_some_and(|times| !times.is_empty())
                })
        };
        let seen = (trees.local().iter())
            .find(|(_, node)| node.identity.ino == ino)
            .is_some_and(|(target, _)| {
                synced.get(target).is_some()
                    && (synced.iter())
                        .filter(|(id, _)| above(id).contains(target) || above(target).contains(id))
                        .all(|(id, shape)| {
                            !pending(id)
                                && server
                                    .get(id)
                                    .is_some_and(|there| !there.trashed && there.shape.same(shape))
                        })
            });
        let settled =
            (self.devices.iter()).all(|device| !device.rolled_back && !device.recovering());
        if seen && settled && device.engine.history == self.space.history_id() {
            let mut deletion = Deletion {
                device: d,
                at: self.clock,
                resendable: (files.iter())
                    .map(|(_, write, _)| *write)
                    .filter(|write| self.exposed.contains_key(write) && !live.contains_key(write))
                    .collect(),
                files,
                reached: false,
                gone: false,
            };
            deletion.reached = deletion.held(&live, |_| false).is_none();
            self.deletions.push(deletion);
        }
    }

    pub(super) fn revived_delete(
        &self,
        nodes: &[(NodeId, RemoteNode)],
        live: &BTreeMap<i64, NodeId>,
    ) -> Result<(), Failure> {
        if !self.revived_deletes {
            return Ok(());
        }
        let parents: BTreeMap<&NodeId, &NodeId> = (nodes.iter())
            .filter(|(_, node)| !node.trashed)
            .map(|(id, node)| (id, &node.shape.parent))
            .collect();
        let dropped = (self.deletions.iter())
            .filter(|deletion| !deletion.reached && !deletion.gone)
            .find_map(|deletion| {
                let write = deletion.held(live, |write| {
                    deletion.resendable.contains(&write) || self.copied.contains(&write)
                })?;
                let node = &live[&write];
                let above: Vec<&NodeId> =
                    std::iter::successors(Some(node), |id| parents.get(id).copied()).collect();
                let changed_since = (0..self.devices.len())
                    .filter(|other| *other != deletion.device)
                    .any(|other| {
                        let inos: BTreeSet<u128> = (above.iter())
                            .filter_map(|id| self.touched_nodes[other].get(*id).copied())
                            .collect();
                        (self.actions[other].range(deletion.at..))
                            .any(|(_, (own, _))| inos.contains(own))
                    });
                (!changed_since).then_some((write, deletion.device))
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
