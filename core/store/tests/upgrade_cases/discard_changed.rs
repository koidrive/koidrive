// SPDX-License-Identifier: Apache-2.0

upgrade_case!(discard_changed, |store| {
    store
        .transaction(|tx| {
            let before = tx.engine_state()?;
            assert!(before.trees.discard_changed.is_empty());
            let report = tx
                .local_nodes()?
                .into_iter()
                .find(|node| node.shape.name == "report.txt")
                .unwrap();
            let mut state = before.clone();
            state.trees.discard_changed.insert(report.node_id.clone());
            tx.set_engine_state(&state)?;
            let mut stored = tx.engine_state()?;
            assert_eq!(
                stored.trees.discard_changed,
                std::collections::BTreeSet::from([report.node_id])
            );
            stored.trees.discard_changed.clear();
            assert_eq!(stored, before);
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
