// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use koidrive_engine::commit::Base;
use koidrive_engine::tree::{Content, NodeId, RemoteNode};

use koidrive_proto::v1::NodeStatus;

use super::{Failure, SpaceDo, Spent, World, tag};

pub(super) fn live_nodes(space: &SpaceDo) -> BTreeMap<i64, BTreeSet<NodeId>> {
    let mut live: BTreeMap<i64, BTreeSet<NodeId>> = BTreeMap::new();
    for node in space.nodes() {
        if node.status != NodeStatus::Live as i32 {
            continue;
        }
        if let Some(content) = node.current.and_then(|version| version.content) {
            live.entry(tag(&content)).or_default().insert(node.node_id);
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
    carried_at: Option<u64>,
}

pub(super) struct Deletion {
    pub(super) device: usize,
    base: Base,
    files: Vec<Deleted>,
    pub(super) reached: bool,
    pub(super) gone: bool,
}

impl Deletion {
    pub(super) fn carried(
        &mut self,
        seq: u64,
        deleted: &BTreeSet<&str>,
        parents: &BTreeMap<&str, &str>,
    ) {
        for file in &mut self.files {
            let above = (file.path.last()).into_iter().flat_map(|root| {
                std::iter::successors(Some(root.as_str()), |id| {
                    parents.get(id).copied().filter(|parent| parent != id)
                })
            });
            if (file.path.iter().map(|id| id.as_str()))
                .chain(above)
                .any(|id| deleted.contains(id))
            {
                file.carried_at.get_or_insert(seq);
            }
        }
    }

    pub(super) fn restored(&mut self, head: u64) {
        for file in &mut self.files {
            file.carried_at = file.carried_at.filter(|seq| *seq <= head);
        }
    }

    pub(super) fn held(
        &self,
        live: &BTreeMap<i64, BTreeSet<NodeId>>,
        skip: impl Fn(&NodeId, &Deleted) -> bool,
    ) -> Option<i64> {
        (self.files.iter()).find_map(|file| {
            let nodes = |write: &i64| live.get(write).into_iter().flatten();
            let at_node = std::iter::once(&file.write)
                .chain(&file.before)
                .filter(|write| nodes(write).any(|there| *there == file.node))
                .map(|write| (*write, &file.node));
            let elsewhere = nodes(&file.write)
                .filter(|there| **there != file.node && !file.elsewhere.contains(*there))
                .map(|there| (file.write, there));
            at_node
                .chain(elsewhere)
                .find(|(_, there)| !skip(there, file))
                .map(|(write, _)| write)
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
                let write = tag(content);
                let before: BTreeSet<i64> =
                    std::iter::successors(self.overwrote.get(&write), |write| {
                        self.overwrote.get(write)
                    })
                    .copied()
                    .collect();
                let holders: Vec<&NodeId> = (synced.iter())
                    .filter(|(id, shape)| {
                        !taken.contains(*id)
                            && (shape.content.as_ref())
                                .is_some_and(|c| tag(c) == write || before.contains(&tag(c)))
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
                    carried_at: None,
                })
            })
            .collect();
        let server: BTreeMap<NodeId, RemoteNode> = (self.space.nodes().iter())
            .filter_map(|state| RemoteNode::from_state(state).ok())
            .collect();
        let scope: Vec<_> = (synced.iter())
            .filter(|(id, _)| under_target(id) || (files.iter()).any(|file| file.path.contains(id)))
            .collect();
        let seen = target.is_some_and(|target| {
            synced.get(target).is_some()
                && (scope.iter()).all(|(id, shape)| {
                    (server.get(*id)).is_some_and(|there| !there.trashed && there.shape.same(shape))
                })
        });
        if !seen {
            return;
        }
        let mut deletion = Deletion {
            device: d,
            base: device.engine.cursor.clone(),
            files,
            reached: false,
            gone: false,
        };
        deletion.reached = deletion.held(&live, |_, _| false).is_none();
        self.deletions.push(deletion);
    }

    fn inos_of(&self, d: usize, node: &NodeId) -> impl Iterator<Item = u128> {
        let local = self.devices[d].engine.trees.local().get(node);
        (local.map(|node| node.identity.ino))
            .into_iter()
            .chain(self.touched_nodes[d].get(node).copied())
    }

    pub(super) fn forget_unsent_deletes(&mut self, d: usize) {
        self.deletions.retain_mut(|deletion| {
            if deletion.reached || deletion.device != d {
                return true;
            }
            (deletion.files).retain(|file| file.carried_at.is_some());
            !deletion.files.is_empty()
        });
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
                let r = self.r_commits(&deletion.base);
                let write = deletion.held(&live, |node, file| {
                    let changed_since = |spent: &Spent| {
                        let is = |id: &NodeId| {
                            self.inos_of(spent.device, id).any(|ino| ino == spent.ino)
                        };
                        let itself = is(node) || is(&file.node);
                        let moved_between = (file.path.iter())
                            .skip(1)
                            .any(|id| spent.reparented.contains(id) && is(id));
                        let revived = spent.revived.contains(node) && (file.path.iter()).any(is);
                        spent.device != deletion.device
                            && (self.actions[spent.device].get(&spent.at))
                                .is_some_and(|(own, _)| *own == spent.ino)
                            && !(self.carriers.get(&(spent.device, spent.ino, spent.at)))
                                .into_iter()
                                .flatten()
                                .any(|(seq, commit)| r.get(seq) == Some(&commit.as_str()))
                            && (itself || moved_between || revived)
                    };
                    self.spent.iter().any(changed_since)
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
