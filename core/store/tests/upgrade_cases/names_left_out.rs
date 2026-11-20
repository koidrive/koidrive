// SPDX-License-Identifier: Apache-2.0

upgrade_case!(names_left_out, |store| {
    store
        .transaction(|tx| {
            let before = tx.engine_state()?;
            assert!(before.trees.names_left_out.is_empty());
            let report = tx
                .local_nodes()?
                .into_iter()
                .find(|node| node.shape.name == "report.txt")
                .unwrap();
            let mut state = before.clone();
            state.trees.names_left_out.insert(report.node_id.clone(), None);
            tx.set_engine_state(&state)?;
            let mut stored = tx.engine_state()?;
            assert_eq!(
                stored.trees.names_left_out,
                std::collections::BTreeMap::from([(report.node_id, None)])
            );
            stored.trees.names_left_out.clear();
            assert_eq!(stored, before);
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
