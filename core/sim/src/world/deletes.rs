// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use koidrive_engine::tree::NodeId;
use koidrive_proto::v1::NodeStatus;

use super::intents::{Check, Field, Intents, Server, Value, expected};
use super::rules::Unsynced;
use super::{Failure, SpaceDo, World, tag};

pub(super) struct Deletion {
    intent: usize,
    device: usize,
    left_out: BTreeSet<NodeId>,
    held: BTreeSet<(i64, NodeId)>,
    removed: BTreeSet<i64>,
}

impl Deletion {
    pub(super) fn new(
        space: &SpaceDo,
        device: usize,
        intent: usize,
        left_out: BTreeSet<NodeId>,
        removed: BTreeSet<i64>,
    ) -> Self {
        let held = (space.nodes().into_iter())
            .filter(|node| node.status == NodeStatus::Live as i32)
            .filter_map(|node| Some((tag(&node.current?.content?), NodeId::from(node.node_id))))
            .collect();
        Deletion {
            intent,
            device,
            left_out,
            held,
            removed,
        }
    }

    fn deleted<'a>(&self, intents: &'a Intents, gone: impl Fn(usize) -> bool) -> Vec<&'a NodeId> {
        let i = self.intent;
        let delete = intents.intent(i);
        let Value::Trashed {
            subtree,
            replaced_by,
            ..
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
        (subtree.iter())
            .flat_map(|ino| intents.deleted_at(i, *ino))
            .collect()
    }

    fn unseen(&self, intents: &Intents, j: usize) -> bool {
        let i = self.intent;
        let other = intents.intent(j);
        j != i
            && other.stands()
            && !other.restore_answer()
            && !matches!(other.value, Value::Trashed { .. })
            && !intents.saw(i, j)
            && !(other.answer.is_some() && intents.saw_value(i, j))
            && (intents.saw(j, i) || !other.lost)
    }

    fn undone(
        &self,
        intents: &Intents,
        server: &Server,
        gone: impl Fn(usize) -> bool,
    ) -> Vec<NodeId> {
        let deleted = self.deleted(intents, gone);
        let kept_for_left_out = |node: &NodeId| {
            (self.left_out.iter()).any(|out| {
                out == node || server.live(out).is_some() && server.above(out).contains(node)
            })
        };
        let live = deleted.iter().filter(|id| server.live(id).is_some());
        live.copied()
            .filter(|node| !kept_for_left_out(node))
            .filter_map(|node| {
                let brings_back = |j: usize| {
                    let other = intents.intent(j);
                    let carries = other.value.field() != Field::Content && other.answer.is_none();
                    intents.nodes(j).any(|id| {
                        id == node
                            || server.live(id).is_some() && server.above(id).contains(node)
                            || carries && deleted.contains(&id) && server.above(node).contains(id)
                    })
                };
                let excused = (0..intents.len()).any(|j| self.unseen(intents, j) && brings_back(j));
                (!excused).then(|| node.clone())
            })
            .collect()
    }

    fn seen_writes_elsewhere(
        &self,
        intents: &Intents,
        server: &Server,
        copied: &BTreeMap<i64, usize>,
        gone: impl Fn(usize) -> bool,
    ) -> Vec<(i64, NodeId, NodeId)> {
        let deleted = self.deleted(intents, gone);
        let Some(root) = deleted.first().and(intents.nodes(self.intent).next()) else {
            return Vec::new();
        };
        let seen = (self.removed.iter().copied()).filter(|write| !copied.contains_key(write));
        let put_there_unseen = |node: &NodeId| {
            (0..intents.len())
                .any(|j| self.unseen(intents, j) && intents.nodes(j).any(|id| id == node))
        };
        seen.flat_map(|write| {
            server
                .live_with(write)
                .map(move |node| (write, node.clone()))
        })
        .filter(|(write, node)| {
            !deleted.contains(&node)
                && !self.held.contains(&(*write, node.clone()))
                && !put_there_unseen(node)
        })
        .map(|(write, node)| (write, root.clone(), node))
        .collect()
    }
}

impl World {
    pub(super) fn record_delete(
        &mut self,
        d: usize,
        ino: u128,
        (gone, under): (BTreeSet<u128>, BTreeMap<u128, u128>),
        removed: BTreeSet<i64>,
        unsynced: Unsynced,
    ) {
        let left_out = (self.devices[d].engine.trees.local().iter())
            .filter(|(_, node)| unsynced.inos.contains(&node.identity.ino))
            .map(|(id, _)| id.clone())
            .chain(unsynced.nodes)
            .collect();
        let value = Value::Trashed {
            subtree: gone.into_iter().collect(),
            under,
            replaced_by: None,
        };
        let intent = self.intents.record(d, self.moment, ino, "delete", value);
        (self.deletions).push(Deletion::new(&self.space, d, intent, left_out, removed));
    }

    pub(super) fn undone_delete(&self, server: &Server) -> Result<(), Failure> {
        let gone = |d: usize| self.devices[d].left;
        for deletion in &self.deletions {
            let d = deletion.device;
            let undone = (deletion.undone(&self.intents, server, gone)).into_iter().map(|node| {
                format!(
                    "the delete of node {node} on device {d} came undone, and no change its user \
                     did not see brought the node back"
                )
            });
            let elsewhere = (deletion.seen_writes_elsewhere(
                &self.intents,
                server,
                &self.copied,
                gone,
            ))
            .into_iter()
            .map(|(write, root, node)| {
                format!(
                    "write {write} that the delete of node {root} on device {d} saw is live \
                         again on node {node}, and no user copied it"
                )
            });
            for why in undone.chain(elsewhere) {
                if !expected(Check::DeleteUndone, &why) {
                    return Err(Failure::Diverged(why));
                }
            }
        }
        Ok(())
    }
}
