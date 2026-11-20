// SPDX-License-Identifier: Apache-2.0

upgrade_case!(discard_members, |store| {
    store
        .transaction(|tx| {
            let mut state = tx.engine_state()?;
            assert!(state.trees.discard_members.is_empty());
            let report = tx
                .local_nodes()?
                .into_iter()
                .find(|node| node.shape.name == "report.txt")
                .unwrap();
            let folder = report.shape.parent_id.clone();
            (state.trees.discard_members).insert(report.node_id.clone(), folder.clone().into());
            tx.set_engine_state(&state)?;
            let members: Vec<_> = (tx.engine_state()?.trees.discard_members.into_iter())
                .map(|(node, folder)| (node, folder.to_string()))
                .collect();
            assert_eq!(members, [(report.node_id, folder)]);
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
