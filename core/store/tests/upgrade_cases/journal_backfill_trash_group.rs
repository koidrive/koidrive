// SPDX-License-Identifier: Apache-2.0

upgrade_case!(journal_backfill_trash_group, |store| {
    use koidrive_engine::journal::{Field, IntentId};
    let node = store
        .transaction(|tx| {
            assert!(tx.journal_backfill_owed()?);
            let root = tx.link()?.unwrap().space_id;
            let remote = (tx.remote_nodes()?.into_iter())
                .find(|remote| remote.parent_id == root)
                .unwrap();
            let node = koidrive_engine::tree::NodeId::from(remote.node_id.as_str());
            let mut state = tx.engine_state()?;
            state.trees.past_positions.insert(
                node.clone(),
                vec![koidrive_engine::state::Agreed {
                    rev: 1,
                    parent: "root".into(),
                    name: "trashed.txt".into(),
                    trash_group: Some("1:0".into()),
                }],
            );
            tx.set_engine_state(&state)?;
            let mut trees = koidrive_engine::classify::Trees::new(
                root.as_str(),
                &koidrive_engine::ids::IdGen::new(1),
            );
            let (_, remote) = koidrive_engine::tree::RemoteNode::from_state(&remote).unwrap();
            trees.set_remote(node.clone(), remote);
            let mut past = koidrive_engine::state::TreesState::default();
            past.past_positions.insert(
                node.clone(),
                tx.engine_state()?.trees.past_positions[&node].clone(),
            );
            past.dirty.insert(node.clone());
            trees.restore(past).unwrap();
            trees.take_changes();
            trees.backfill(None, false, 1_800_000_000_000);
            tx.write_engine_changes(&koidrive_store::EngineChanges {
                trees: trees.take_changes(),
                commits: Default::default(),
                effects: Default::default(),
            })?;
            tx.journal_backfilled()?;
            Ok::<_, koidrive_store::Error>(node)
        })
        .unwrap();
    store
        .read(|tx| {
            assert!(!tx.journal_backfill_owed()?);
            let state = tx.engine_state()?;
            assert_eq!(
                state.trees.past_positions[&node][0].trash_group.as_deref(),
                Some("1:0")
            );
            let status = &state.trees.journal.history[&(node.clone(), Field::Status)];
            let trashed = IntentId::legacy_trashed(&node.to_string(), "1:0");
            assert!(
                status.iter().any(|entry| entry.intent == trashed),
                "{status:?}"
            );
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
