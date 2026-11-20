// SPDX-License-Identifier: Apache-2.0

upgrade_case!(move_bases, |store| {
    use koidrive_engine::commit::InFlight;
    use koidrive_engine::ids::IdGen;
    use koidrive_engine::state::{Agreed, MoveHistory, MovePosition, MovedHere};
    use koidrive_engine::tree::{NodeId, RemoteNode};
    use koidrive_proto::v1::{Commit, FieldIntents, Intent, Move, Op, op};
    use std::collections::BTreeMap;

    store
        .transaction(|tx| {
            let mut state = tx.engine_state()?;
            assert!(
                state
                    .trees
                    .moved_here
                    .values()
                    .all(|moved| moved.history.is_none())
            );
            if let Some(flight) = state.commits.in_flight.as_ref() {
                assert!(flight.move_bases.is_empty());
            }
            let nodes = tx.remote_nodes()?;
            assert!(nodes.iter().all(|node| RemoteNode::place_id(node).is_none()));
            let mut remote = nodes.into_iter().find(|node| node.name == "report.txt").unwrap();
            let node = NodeId::from(&remote.node_id);
            let mut ids = IdGen::new(7);
            let mut at = |rev, name: &str, trash_group| MovePosition {
                place_id: ids.uuid_v7_bytes(rev),
                record: Agreed {
                    rev,
                    parent: (&remote.parent_id).into(),
                    name: name.into(),
                    trash_group,
                },
            };
            let history = MoveHistory {
                result: at(4, "report.txt", None),
                bases: vec![at(3, "old", None), at(2, "earlier", Some("delete:0".into()))],
            };
            remote.intents = Some(FieldIntents {
                place: Some(Intent { id: history.result.place_id.to_vec(), ..Default::default() }),
                ..Default::default()
            });
            tx.put_remote_node(&remote)?;
            assert_eq!(tx.remote_nodes()?.into_iter().find(|n| n.node_id == remote.node_id), Some(remote.clone()));
            state.trees.moved_here.insert(
                node.clone(),
                MovedHere {
                    seq: 4,
                    upgraded: false,
                    history: Some(Box::new(history.clone())),
                },
            );
            state.commits.in_flight = Some(InFlight {
                commit: Commit {
                    ops: vec![Op {
                        op: Some(op::Op::Move(Move {
                            node_id: node.to_string(),
                            ..Default::default()
                        })),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                move_bases: BTreeMap::from([(node, history.bases)]),
                deletes: BTreeMap::new(),
                restores: BTreeMap::new(),
                owners: BTreeMap::new(),
                result: None,
            });
            tx.set_engine_state(&state)?;
            assert_eq!(tx.engine_state()?, state);
            state.commits.in_flight = None;
            tx.set_engine_state(&state)?;
            assert_eq!(tx.engine_state()?, state);
            remote.intents = None;
            tx.put_remote_node(&remote)?;
            assert!(tx.remote_nodes()?.iter().all(|node| RemoteNode::place_id(node).is_none()));
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
