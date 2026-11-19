// SPDX-License-Identifier: Apache-2.0

upgrade_case!(lost_shapes, |store| {
    store
        .transaction(|tx| {
            let mut state = tx.engine_state()?;
            assert!(state.trees.lost_shapes.is_empty());
            let report = tx
                .local_nodes()?
                .into_iter()
                .find(|node| node.shape.name == "report.txt")
                .unwrap();
            let shape = koidrive_engine::tree::Shape {
                parent: report.shape.parent_id.into(),
                name: report.shape.name,
                kind: koidrive_engine::tree::Kind::File,
                content: report.shape.content,
            };
            state
                .trees
                .lost_shapes
                .insert(report.node_id.clone(), shape.clone());
            tx.set_engine_state(&state)?;
            assert_eq!(
                tx.engine_state()?.trees.lost_shapes,
                std::collections::BTreeMap::from([(report.node_id, shape)])
            );
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
