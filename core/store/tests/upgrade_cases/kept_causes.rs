// SPDX-License-Identifier: Apache-2.0

upgrade_case!(kept_causes, |store| {
    store
        .transaction(|tx| {
            let mut state = tx.engine_state()?;
            let report = tx
                .local_nodes()?
                .into_iter()
                .find(|node| node.shape.name == "report.txt")
                .unwrap();
            let cause = koidrive_engine::state::KeptCause {
                path: vec!["report.txt".into()],
                rules: Vec::new(),
            };
            state.trees.kept_nodes.insert(report.node_id.clone(), Some(cause.clone()));
            state.trees.kept_nodes.insert("left".into(), None);
            tx.set_engine_state(&state)?;
            let stored = tx.engine_state()?.trees.kept_nodes;
            assert_eq!(stored.get(&report.node_id), Some(&Some(cause)));
            assert_eq!(stored.get("left"), Some(&None));
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
