// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeSet;

use koidrive_engine::tree::NodeId;

use super::intents::{Check, Field, Intents, Server, Value, expected};
use super::rules::Unsynced;
use super::{Failure, World};

pub(super) struct Deletion {
    pub(super) intent: usize,
    pub(super) device: usize,
    pub(super) left_out: BTreeSet<NodeId>,
}

impl Deletion {
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
        let deleted: Vec<&NodeId> = (subtree.iter())
            .flat_map(|ino| intents.deleted_at(i, *ino))
            .collect();
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
                let excused = (0..intents.len()).any(|j| {
                    let other = intents.intent(j);
                    j != i
                        && other.stands()
                        && !other.restore_answer()
                        && !matches!(other.value, Value::Trashed { .. })
                        && !intents.saw(i, j)
                        && !(other.answer.is_some() && intents.saw_value(i, j))
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
        (gone, under): (BTreeSet<u128>, BTreeMap<u128, u128>),
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
        self.deletions.push(Deletion {
            intent,
            device: d,
            left_out,
        });
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
