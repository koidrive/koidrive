// SPDX-License-Identifier: Apache-2.0

upgrade_case!(full_history, |store| {
    store
        .transaction(|tx| {
            let mut state = tx.engine_state()?;
            assert!(state.trees.full_history.is_empty());
            let report = tx
                .local_nodes()?
                .into_iter()
                .find(|node| node.shape.name == "report.txt")
                .unwrap();
            state.trees.full_history.insert(report.node_id.clone());
            tx.set_engine_state(&state)?;
            assert_eq!(
                tx.engine_state()?.trees.full_history,
                std::collections::BTreeSet::from([report.node_id])
            );
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
