// SPDX-License-Identifier: Apache-2.0

upgrade_case!(moved_in_after_discard, |store| {
    store
        .transaction(|tx| {
            let mut state = tx.engine_state()?;
            assert!(state.trees.moved_in_after_discard.is_empty());
            let report = tx
                .local_nodes()?
                .into_iter()
                .find(|node| node.shape.name == "report.txt")
                .unwrap();
            state.trees.moved_in_after_discard.insert(report.node_id.clone());
            tx.set_engine_state(&state)?;
            assert_eq!(
                tx.engine_state()?.trees.moved_in_after_discard,
                std::collections::BTreeSet::from([report.node_id])
            );
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
