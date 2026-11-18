// SPDX-License-Identifier: Apache-2.0

upgrade_case!(split_trash, |store| {
    store
        .transaction(|tx| {
            let before = tx.engine_state()?;
            assert!(before.commits.trash_groups.is_empty());
            let report = tx.local_nodes()?.into_iter().find(|node| node.shape.name == "report.txt").unwrap();
            let mut after = before.clone();
            after.commits.trash_groups.insert(report.node_id.clone(), "0190a6b2-0000-7000-8000-000000000001:3".into());
            tx.update_engine_state(&before, &after)?;
            assert_eq!(tx.engine_state()?, after);
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
