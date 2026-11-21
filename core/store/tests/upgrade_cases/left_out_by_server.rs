// SPDX-License-Identifier: Apache-2.0

upgrade_case!(left_out_by_server, |store| {
    store
        .transaction(|tx| {
            let before = tx.engine_state()?;
            assert!(before.trees.left_out_by_server.is_empty());
            let report = tx
                .local_nodes()?
                .into_iter()
                .find(|node| node.shape.name == "report.txt")
                .unwrap();
            let mut state = before.clone();
            state.trees.unobserved.insert(report.node_id.clone());
            state.trees.left_out_by_server.insert(report.node_id.clone());
            tx.set_engine_state(&state)?;
            let mut stored = tx.engine_state()?;
            assert_eq!(
                stored.trees.left_out_by_server,
                std::collections::BTreeSet::from([report.node_id])
            );
            stored.trees.unobserved = before.trees.unobserved.clone();
            stored.trees.left_out_by_server.clear();
            assert_eq!(stored, before);
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
