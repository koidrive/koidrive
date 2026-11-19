// SPDX-License-Identifier: Apache-2.0

upgrade_case!(waiting_update_released, |store| {
    store
        .transaction(|tx| {
            let before = tx.engine_state()?;
            let report = tx.local_nodes()?.into_iter().find(|node| node.shape.name == "report.txt").unwrap();
            let mut after = before.clone();
            let base = koidrive_engine::tree::Shape {
                parent: report.shape.parent_id.clone().into(),
                name: "report.txt".into(),
                kind: koidrive_engine::tree::Kind::File,
                content: None,
            };
            let released = koidrive_engine::classify::Waiting {
                shape: Some(base),
                origin: koidrive_engine::classify::Origin::Released,
            };
            after.trees.waiting.insert(report.node_id.clone(), released);
            tx.update_engine_state(&before, &after)?;
            assert_eq!(tx.engine_state()?, after);
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
