// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use koidrive_engine::tree::{Content, NodeId, RemoteNode};

use koidrive_proto::v1::NodeStatus;

use super::intents::{Check, Field, Intents, Server, Value, expected};
use super::rules::Unsynced;
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
    carried_at: Option<u64>,
}

pub(super) struct Deletion {
    intent: usize,
    pub(super) device: usize,
    files: Vec<Deleted>,
    pub(super) unsent: BTreeSet<usize>,
    resendable: BTreeSet<i64>,
    around_restore: bool,
    pub(super) watched: bool,
    pub(super) reached: bool,
    pub(super) gone: bool,
    left_out: BTreeSet<NodeId>,
}

impl Deletion {
    pub(super) fn unwatched(device: usize, intent: usize) -> Self {
        Deletion {
            intent,
            device,
            files: Vec::new(),
            unsent: BTreeSet::new(),
            resendable: BTreeSet::new(),
            around_restore: false,
            watched: false,
            reached: false,
            gone: false,
            left_out: BTreeSet::new(),
        }
    }

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
        self.around_restore = true;
        for file in &mut self.files {
            file.carried_at = file.carried_at.filter(|seq| *seq <= head);
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

    fn undone(
        &self,
        intents: &Intents,
        server: &Server,
        gone: impl Fn(usize) -> bool,
    ) -> Vec<NodeId> {
        let i = self.intent;
        let delete = intents.intent(i);
        let Value::Trashed {
            subtree,
            replaced_by,
        } = &delete.value
        else {
            return Vec::new();
        };
        let author = delete.author;
        if delete.dropped || delete.forgotten || gone(author) {
            return Vec::new();
        }
        let roots: BTreeSet<&NodeId> = intents.nodes(i).collect();
        let saved =
            (replaced_by.iter()).flat_map(|ino| intents.nodes_of(author, *ino, delete.made));
        if roots.is_empty() || saved.into_iter().any(|id| roots.contains(id)) {
            return Vec::new();
        }
        let deleted: Vec<&NodeId> = (subtree.iter())
            .flat_map(|ino| intents.nodes_of(author, *ino, delete.made))
            .collect();
        let kept_for_left_out = |node: &NodeId| {
            (self.left_out.iter()).any(|out| {
                out == node
                    || server.live(out).is_some() && server.above(out).contains(&node.as_str())
            })
        };
        let live = deleted.iter().filter(|id| server.live(id).is_some());
        live.copied()
            .filter(|node| !kept_for_left_out(node))
            .filter_map(|node| {
                let brings_back = |j: usize| {
                    let other = intents.intent(j);
                    let carries = other.value.field() != Field::Content && !other.answer;
                    intents.nodes(j).any(|id| {
                        id == node
                            || server.live(id).is_some()
                                && server.above(id).contains(&node.as_str())
                            || carries
                                && deleted.contains(&id)
                                && server.above(node).contains(&id.as_str())
                    })
                };
                let excused = (0..intents.len()).any(|j| {
                    let other = intents.intent(j);
                    j != i
                        && other.stands()
                        && !other.restore_answer()
                        && !matches!(other.value, Value::Trashed { .. })
                        && !intents.saw(i, j)
                        && !(other.answer && intents.saw_value(i, j))
                        && (intents.saw(j, i) || !other.lost)
                        && brings_back(j)
                });
                (!excused).then(|| node.clone())
            })
            .collect()
    }
}

impl World {
    pub(super) fn record_delete(
        &mut self,
        d: usize,
        ino: u128,
        gone: BTreeSet<u128>,
        deleted: &[Content],
        unsynced: Unsynced,
    ) {
        let device = &self.devices[d];
        let left_out = (device.engine.trees.local().iter())
            .filter(|(_, node)| unsynced.inos.contains(&node.identity.ino))
            .map(|(id, _)| id.clone())
            .chain(unsynced.nodes)
            .collect();
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
        let unsent = (0..self.intents.len())
            .filter(|j| {
                let other = self.intents.intent(*j);
                other.author != d
                    && other.carried.is_none()
                    && !other.dropped
                    && !self.intents.replaced(*j)
            })
            .collect();
        let seen = target.is_some_and(|target| {
            synced.get(target).is_some()
                && (synced.iter())
                    .filter(|(id, _)| {
                        under_target(id) || (files.iter()).any(|file| file.path.contains(id))
                    })
                    .all(|(id, shape)| {
                        (server.get(id))
                            .is_some_and(|there| !there.trashed && there.shape.same(shape))
                    })
        });
        let watched = seen
            && !device.rolled_back
            && !device.recovering()
            && device.engine.history == self.space.history_id();
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
            unsent,
            resendable,
            around_restore: self.restored_to.is_some(),
            watched,
            reached: false,
            gone: false,
            left_out,
        };
        deletion.reached = deletion.held(&live, |_, _, _, _| false).is_none();
        self.deletions.push(deletion);
    }

    pub(super) fn forget_restored_deletes<'a>(
        &mut self,
        d: usize,
        restored: impl IntoIterator<Item = &'a NodeId>,
    ) {
        let restored: BTreeSet<&NodeId> = restored.into_iter().collect();
        for deletion in (self.deletions.iter_mut()).filter(|deletion| deletion.device == d) {
            (deletion.files).retain(|file| !restored.contains(&file.node));
            deletion.watched &= !deletion.files.is_empty();
        }
    }

    pub(super) fn forget_unsent_deletes(&mut self, d: usize) {
        for deletion in &mut self.deletions {
            if deletion.reached || deletion.device != d {
                continue;
            }
            (deletion.files).retain(|file| file.carried_at.is_some());
            deletion.watched &= !deletion.files.is_empty();
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
                let at = self.intents.intent(deletion.intent).made;
                let write = deletion.held(&live, |write, node, file, at_node| {
                    let changed_since = |j: usize| {
                        let other = self.intents.intent(j);
                        let Some(carried) = (other.carried.as_ref()).filter(|_| {
                            other.author != deletion.device
                                && !other.answer
                                && !other.restore_answer()
                                && (deletion.unsent.contains(&j) || other.made >= at)
                        }) else {
                            return false;
                        };
                        let is = |id: &NodeId| self.intents.nodes(j).any(|there| there == id);
                        let itself = is(node) || is(&file.node);
                        let moved_between = (file.path.iter())
                            .skip(1)
                            .any(|id| carried.reparented.contains(id) && is(id));
                        let revived = carried.revived.contains(node) && (file.path.iter()).any(is);
                        itself || moved_between || revived
                    };
                    !deletion.around_restore
                        || deletion.resendable.contains(&write)
                        || (!at_node && self.copied.contains_key(&write))
                        || (0..self.intents.len()).any(changed_since)
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

    pub(super) fn undone_delete(&self, server: &Server) -> Result<(), Failure> {
        let gone = |d: usize| self.devices[d].left;
        for deletion in &self.deletions {
            let d = deletion.device;
            for node in deletion.undone(&self.intents, server, gone) {
                let why = format!(
                    "the delete of node {node} on device {d} came undone, and no change its user \
                     did not see brought the node back"
                );
                if !expected(Check::DeleteUndone, &why) {
                    return Err(Failure::Diverged(why));
                }
            }
        }
        Ok(())
    }
}
