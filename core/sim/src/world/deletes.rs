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
        gone: impl Fn(usize) -> bool,
    ) -> Vec<(i64, NodeId, NodeId)> {
        let deleted = self.deleted(intents, gone);
        let Some(root) = deleted.first().and(intents.nodes(self.intent).next()) else {
            return Vec::new();
        };
        let put_there = |write: i64, node: &NodeId| {
            (0..intents.len()).any(|j| {
                j != self.intent
                    && intents.nodes(j).any(|id| id == node)
                    && match intents.intent(j).value {
                        Value::Content { write: put, .. } => {
                            put == write && (intents.intent(j).copies() || self.unseen(intents, j))
                        }
                        Value::Live => self.unseen(intents, j),
                        Value::Place { .. } => {
                            self.unseen(intents, j) && intents.reached_trashed(j, node)
                        }
                        Value::Trashed { .. } => false,
                    }
            })
        };
        (self.removed.iter().copied())
            .flat_map(|write| {
                server
                    .live_with(write)
                    .map(move |node| (write, node.clone()))
            })
            .filter(|(write, node)| {
                !deleted.contains(&node)
                    && !self.held.contains(&(*write, node.clone()))
                    && !put_there(*write, node)
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
            let elsewhere = (deletion.seen_writes_elsewhere(&self.intents, server, gone))
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

#[cfg(test)]
mod tests {
    use koidrive_proto::v1::{NodeKind, NodeState, VersionInfo};

    use super::super::intents::{Answered, KEEP};
    use super::*;

    fn file(root: &str, id: &str, status: NodeStatus, write: i64) -> NodeState {
        NodeState {
            node_id: id.into(),
            parent_id: root.into(),
            name: id.into(),
            kind: NodeKind::File as i32,
            status: status as i32,
            current: Some(VersionInfo {
                content: Some(super::super::content(0, 0, write)),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn deleted_x() -> (Intents, Deletion) {
        let world = World::new(2);
        let mut intents = Intents::new(2);
        let root = NodeId::from("root");
        let disk = |d: usize| &world.devices[d].disk;
        intents.observe(0, 0, disk(0), &BTreeMap::from([(1, "x".into())]), &root);
        intents.observe(1, 0, disk(1), &BTreeMap::from([(2, "z".into())]), &root);
        let intent = intents.answered_delete(0, 1, 1, &"x".into());
        let deletion = Deletion {
            intent,
            device: 0,
            left_out: BTreeSet::new(),
            held: BTreeSet::from([(7, "x".into())]),
            removed: BTreeSet::from([7]),
        };
        (intents, deletion)
    }

    fn x_trashed_and_z_holding_7(z_name: &str) -> Server {
        let mut z = file("root", "z", NodeStatus::Live, 7);
        z.name = z_name.into();
        Server::of("root", vec![file("root", "x", NodeStatus::Trashed, 7), z])
    }

    #[test]
    fn a_copy_or_a_keep_answer_excuses_a_write_a_delete_saw_only_on_the_node_it_put_it_on() {
        let copy = |intents: &mut Intents, write| {
            intents.record(0, 0, 3, "copy", write);
        };
        let keep = |intents: &mut Intents, write| {
            intents.answered((0, 0, KEEP), 3, write, Answered::Owed, true);
        };
        for (how, put) in [
            ("copy", &copy as &dyn Fn(&mut Intents, Value)),
            ("keep", &keep),
        ] {
            for (copied_to, reported) in [("y", true), ("z", false)] {
                let (mut intents, deletion) = deleted_x();
                let world = World::new(2);
                let copy = BTreeMap::from([(3, NodeId::from(copied_to))]);
                intents.observe(0, 0, &world.devices[0].disk, &copy, &"root".into());
                let write = Value::Content {
                    write: 7,
                    digest: super::super::content(0, 0, 7).manifest_digest,
                };
                put(&mut intents, write);
                let tree = x_trashed_and_z_holding_7("z");
                let found = deletion.seen_writes_elsewhere(&intents, &tree, |_| false);
                let wanted = match reported {
                    true => vec![(7, NodeId::from("x"), NodeId::from("z"))],
                    false => Vec::new(),
                };
                assert_eq!(found, wanted, "{how} to {copied_to}");
            }
        }
    }

    #[test]
    fn an_unseen_rename_does_not_explain_a_write_a_delete_saw_on_another_node() {
        let (mut intents, deletion) = deleted_x();
        let wanted = vec![(7, NodeId::from("x"), NodeId::from("z"))];
        let tree = x_trashed_and_z_holding_7("z");
        assert_eq!(
            deletion.seen_writes_elsewhere(&intents, &tree, |_| false),
            wanted
        );
        let rename = Value::Place {
            parent: None,
            name: "renamed".into(),
        };
        intents.record(1, 2, 2, "move", rename);
        let tree = x_trashed_and_z_holding_7("renamed");
        intents.sync(&tree, 3, None);
        assert_eq!(
            deletion.seen_writes_elsewhere(&intents, &tree, |_| false),
            wanted
        );
    }

    #[test]
    fn a_delete_answer_reports_the_write_it_showed_when_it_is_live_on_another_node() {
        use koidrive_engine::commit::Commits;
        use koidrive_engine::question::{Held, Question, QuestionKind, RecoveryGroup, Revisions};
        use koidrive_engine::tree::RemoteNode;

        let mut world = World::new(1);
        let root = world.space.space_id().to_owned();
        let x = NodeId::from("x");
        let (_, remote) = RemoteNode::from_state(&file(&root, "x", NodeStatus::Live, 7)).unwrap();
        let inos = BTreeMap::from([(1, x.clone())]);
        let disk = &world.devices[0].disk;
        (world.intents).observe(0, 0, disk, &inos, &NodeId::from(root.as_str()));
        let device = &mut world.devices[0];
        device.down = true;
        device.store.trees.set_remote(x.clone(), remote);
        let mut state = device.store.commits.state();
        let members = BTreeMap::from([(
            x.clone(),
            Some(Revisions {
                content_rev: 0,
                meta_rev: 0,
            }),
        )]);
        state.questions.push(Question {
            id: "q".into(),
            asked_at_ms: 1,
            kind: QuestionKind::Recovery {
                group: RecoveryGroup::DeletedOnThisDevice,
                sent_in: None,
                cause: None,
            },
            held: BTreeMap::from([(x, Held { members, files: 1 })]),
            answer: None,
            shown: BTreeMap::new(),
            frozen: None,
        });
        device.store.commits = Commits::from_state(state).unwrap();
        assert!(world.answer(0, 0, false).unwrap());
        let tree = Server::of(
            &root,
            vec![
                file(&root, "x", NodeStatus::Trashed, 7),
                file(&root, "z", NodeStatus::Live, 7),
            ],
        );
        let Err(Failure::Diverged(why)) = world.undone_delete(&tree) else {
            panic!("the delete answer let write 7 stay live on node z");
        };
        assert!(
            why.starts_with(
                "write 7 that the delete of node x on device 0 saw is live again on node z"
            ),
            "{why}"
        );
    }
}
