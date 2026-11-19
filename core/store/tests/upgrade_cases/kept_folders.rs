// SPDX-License-Identifier: Apache-2.0

upgrade_case!(kept_folders, |store| {
    store
        .transaction(|tx| {
            let mut state = tx.engine_state()?;
            assert!(state.trees.kept_folders.is_empty());
            assert!(state.trees.kept_entries.is_empty());
            assert!(state.trees.kept_nodes.is_empty());
            let report = tx
                .local_nodes()?
                .into_iter()
                .find(|node| node.shape.name == "report.txt")
                .unwrap();
            state.trees.kept_folders.insert(NodeId::from(&report.node_id));
            let kept = koidrive_engine::state::KeptEntries {
                names: [".env".into()].into(),
                moved: Default::default(),
                listed: true,
                causes: Default::default(),
            };
            state.trees.kept_entries.insert(NodeId::from(&report.node_id), kept.clone());
            state.trees.kept_nodes.insert(NodeId::from(&report.node_id), None);
            tx.set_engine_state(&state)?;
            let stored = tx.engine_state()?.trees;
            let report = std::collections::BTreeSet::from([NodeId::from(report.node_id)]);
            assert_eq!(stored.kept_folders, report);
            assert_eq!(stored.kept_entries.keys().cloned().collect::<std::collections::BTreeSet<_>>(), report);
            assert_eq!(stored.kept_entries.values().collect::<Vec<_>>(), [&kept]);
            assert_eq!(stored.kept_nodes, report.iter().map(|node| (node.clone(), None)).collect());
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
