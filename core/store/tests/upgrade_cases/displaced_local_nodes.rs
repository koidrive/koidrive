// SPDX-License-Identifier: Apache-2.0

upgrade_case!(displaced_local_nodes, |store| {
    store
        .transaction(|tx| {
            let node = tx.local_nodes()?[0].node_id.clone();
            let mut state = tx.engine_state()?;
            state.trees.outside.insert(node.clone());
            state.trees.lost.insert(node.clone());
            let identity = koidrive_engine::tree::FileIdentity {
                ino: 9,
                birth_ns: Some(-1),
                handle: None,
            };
            state.trees.displaced.insert(node, identity);
            tx.set_engine_state(&state)?;
            let stored = tx.engine_state()?.trees;
            assert_eq!(
                (stored.outside, stored.lost, stored.displaced),
                (state.trees.outside, state.trees.lost, state.trees.displaced)
            );
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
