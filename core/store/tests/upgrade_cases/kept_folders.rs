// SPDX-License-Identifier: Apache-2.0

upgrade_case!(kept_folders, |store| {
    store
        .transaction(|tx| {
            let mut state = tx.engine_state()?;
            assert!(state.trees.kept_folders.is_empty());
            assert!(state.trees.kept_entries.is_empty());
            let report = tx
                .local_nodes()?
                .into_iter()
                .find(|node| node.shape.name == "report.txt")
                .unwrap();
            state.trees.kept_folders.insert(report.node_id.clone());
            let kept = koidrive_engine::state::KeptEntries {
                names: [".env".into()].into(),
                listed: true,
            };
            state.trees.kept_entries.insert(report.node_id.clone(), kept.clone());
            tx.set_engine_state(&state)?;
            let stored = tx.engine_state()?.trees;
            let report = std::collections::BTreeSet::from([report.node_id]);
            assert_eq!(stored.kept_folders, report);
            assert_eq!(stored.kept_entries.keys().cloned().collect::<std::collections::BTreeSet<_>>(), report);
            assert_eq!(stored.kept_entries.values().collect::<Vec<_>>(), [&kept]);
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
