// SPDX-License-Identifier: Apache-2.0

upgrade_case!(unsent_creates, |store| {
    store
        .transaction(|tx| {
            let mut state = tx.engine_state()?;
            assert!(state.trees.unsent_creates.is_empty());
            let report = tx
                .local_nodes()?
                .into_iter()
                .find(|node| node.shape.name == "report.txt")
                .unwrap();
            state.trees.unsent_creates.insert(NodeId::from(&report.node_id));
            tx.set_engine_state(&state)?;
            assert_eq!(
                tx.engine_state()?.trees.unsent_creates,
                std::collections::BTreeSet::from([NodeId::from(report.node_id)])
            );
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
