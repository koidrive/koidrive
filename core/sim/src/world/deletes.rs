// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use koidrive_engine::tree::{Content, NodeId, RemoteNode};

use koidrive_proto::v1::NodeStatus;

use super::intents::{Check, Field, Intents, Server, Value, expected};
use super::{Failure, SpaceDo, World, tag};

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
}

pub(super) struct Deletion {
    intent: usize,
    pub(super) device: usize,
    files: Vec<Deleted>,
    resendable: BTreeSet<i64>,
    pub(super) watched: bool,
    pub(super) reached: bool,
    pub(super) gone: bool,
}

impl Deletion {
    pub(super) fn unwatched(device: usize, intent: usize) -> Self {
        Deletion {
            intent,
            device,
            files: Vec::new(),
            resendable: BTreeSet::new(),
            watched: false,
            reached: false,
            gone: false,
        }
    }

    pub(super) fn held(
        &self,
        live: &BTreeMap<i64, BTreeSet<NodeId>>,
        skip: impl Fn(i64, bool) -> bool,
    ) -> Option<(i64, NodeId, &Deleted)> {
        (self.files.iter()).find_map(|file| {
            let nodes = |write: &i64| live.get(write).into_iter().flatten();
            let at_node = std::iter::once(&file.write)
                .chain(&file.before)
                .filter(|write| nodes(write).any(|there| *there == file.node))
                .map(|write| (*write, file.node.clone(), true));
            let elsewhere = nodes(&file.write)
                .filter(|there| **there != file.node && !file.elsewhere.contains(*there))
                .map(|there| (file.write, there.clone(), false));
            at_node
                .chain(elsewhere)
                .find(|(write, _, at_node)| !skip(*write, *at_node))
                .map(|(write, there, _)| (write, there, file))
        })
    }

    fn undone(
        &self,
        intents: &Intents,
        server: &Server,
        gone: impl Fn(usize) -> bool,
    ) -> Option<NodeId> {
        let i = self.intent;
        let delete = intents.intent(i);
        let Value::Trashed {
            subtree,
            replaced_by,
        } = &delete.value
        else {
            return None;
        };
        let author = delete.author;
        if delete.dropped || delete.forgotten || gone(author) {
            return None;
        }
        let roots: BTreeSet<&NodeId> = intents.nodes(i).collect();
        let saved = (replaced_by.iter()).flat_map(|ino| intents.nodes_of(author, *ino));
        if roots.is_empty() || saved.into_iter().any(|id| roots.contains(id)) {
            return None;
        }
        let deleted: Vec<&NodeId> = (subtree.iter())
            .flat_map(|ino| intents.nodes_of(author, *ino))
            .collect();
        let live = deleted.iter().filter(|id| server.live(id).is_some());
        live.copied().find_map(|node| {
            let brings_back = |j: usize| {
                let other = intents.intent(j);
                let carries = other.value.field() != Field::Content && !other.answer;
                intents.nodes(j).any(|id| {
                    id == node
                        || server.live(id).is_some() && server.above(id).contains(&node.as_str())
                        || carries
                            && deleted.contains(&id)
                            && server.above(node).contains(&id.as_str())
                })
            };
            let excused = (0..intents.len()).any(|j| {
                let other = intents.intent(j);
                j != i
                    && !other.dropped
                    && !matches!(other.value, Value::Trashed { .. })
                    && (!other.answer || other.made > delete.made)
                    && (intents.saw(j, i) || !intents.saw(i, j) && !other.lost)
                    && brings_back(j)
            });
            (!excused).then(|| node.clone())
        })
    }
}

impl World {
    pub(super) fn record_delete(
        &mut self,
        d: usize,
        ino: u128,
        gone: BTreeSet<u128>,
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
        let live = live_nodes(&self.space);
        let files: Vec<Deleted> = (deleted.iter())
            .filter_map(|content| {
                let write = tag(content);
                let holders: Vec<&NodeId> = (synced.iter())
                    .filter(|(_, shape)| {
                        (shape.content.as_ref()).is_some_and(|c| tag(c) == write)
                    })
                    .map(|(id, _)| id)
                    .collect();
                let node = (holders.iter())
                    .find(|id| target.is_some_and(|target| above(id).contains(target)))
                    .or(holders.last())
                    .copied()?;
                Some(Deleted {
                    node: node.clone(),
                    write,
                    before: std::iter::successors(self.overwrote.get(&write), |write| {
                        self.overwrote.get(write)
                    })
                    .copied()
                    .collect(),
                    path: std::iter::successors(Some(node.clone()), |id| {
                        let parent = &trees.local().get(id)?.shape.parent;
                        (trees.local().get(parent))
                            .is_some_and(|node| gone.contains(&node.identity.ino))
                            .then(|| parent.clone())
                    })
                    .collect(),
                    elsewhere: (live.get(&write).into_iter().flatten())
                        .filter(|there| *there != node)
                        .cloned()
                        .collect(),
                })
            })
            .collect();
        let server: BTreeMap<NodeId, RemoteNode> = (self.space.nodes().iter())
            .filter_map(|state| RemoteNode::from_state(state).ok())
            .collect();
        let pending = |id: &NodeId| {
            (0..self.intents.len()).any(|j| {
                let other = self.intents.intent(j);
                other.author != d
                    && other.applied == 0
                    && !other.dropped
                    && !self.intents.replaced(j)
                    && self.intents.nodes(j).any(|node| node == id)
            })
        };
        let seen = target.is_some_and(|target| {
            synced.get(target).is_some()
                && (synced.iter())
                    .filter(|(id, _)| {
                        above(id).contains(target)
                            || (files.iter()).any(|file| file.path.contains(id))
                    })
                    .all(|(id, shape)| {
                        !pending(id)
                            && server
                                .get(id)
                                .is_some_and(|there| !there.trashed && there.shape.same(shape))
                    })
        });
        let settled =
            (self.devices.iter()).all(|device| !device.rolled_back && !device.recovering());
        let watched = seen && settled && device.engine.history == self.space.history_id();
        let resendable = (files.iter())
            .map(|file| file.write)
            .filter(|write| self.exposed.contains_key(write) && !live.contains_key(write))
            .collect();
        let value = Value::Trashed {
            subtree: gone.into_iter().collect(),
            replaced_by: None,
        };
        let mut deletion = Deletion {
            intent: self.intents.record(d, self.moment, ino, "delete", value),
            device: d,
            files,
            resendable,
            watched,
            reached: false,
            gone: false,
        };
        deletion.reached = deletion.held(&live, |_, _| false).is_none();
        self.deletions.push(deletion);
    }

    pub(super) fn forget_restored_deletes<'a>(
        &mut self,
        d: usize,
        restored: impl IntoIterator<Item = &'a NodeId>,
    ) {
        let restored: BTreeSet<&NodeId> = restored.into_iter().collect();
        for deletion in &mut self.deletions {
            deletion.watched &= deletion.device != d
                || !(deletion.files.iter()).any(|file| restored.contains(&file.node));
        }
    }

    pub(super) fn revived_delete(&self) -> Result<(), Failure> {
        if !self.revived_deletes {
            return Ok(());
        }
        let live = live_nodes(&self.space);
        let dropped = (self.deletions.iter())
            .filter(|deletion| deletion.watched && !deletion.reached && !deletion.gone)
            .find_map(|deletion| {
                let (write, node, file) = deletion.held(&live, |write, at_node| {
                    deletion.resendable.contains(&write)
                        || (!at_node && self.copied.contains(&write))
                })?;
                let path: Vec<&NodeId> = std::iter::once(&node).chain(&file.path).collect();
                let at = self.intents.intent(deletion.intent).made;
                let changed_since = (0..self.intents.len()).any(|j| {
                    let other = self.intents.intent(j);
                    other.author != deletion.device
                        && !other.answer
                        && other.made >= at
                        && self.intents.nodes(j).any(|id| path.contains(&id))
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

    pub(super) fn undone_delete(&self, server: &Server) -> Result<(), Failure> {
        let gone = |d: usize| self.devices[d].left;
        for deletion in &self.deletions {
            let Some(node) = deletion.undone(&self.intents, server, gone) else {
                continue;
            };
            let d = deletion.device;
            let why = format!(
                "the delete of node {node} on device {d} came undone, and no change its user did \
                 not see brought the node back"
            );
            if expected(Check::DeleteUndone, &why) {
                continue;
            }
            return Err(Failure::Diverged(why));
        }
        Ok(())
    }
}
