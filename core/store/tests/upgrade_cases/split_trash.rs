// SPDX-License-Identifier: Apache-2.0

upgrade_case!(split_trash, |store| {
    store
        .transaction(|tx| {
            let before = tx.engine_state()?;
            assert!(before.commits.trash_groups.is_empty());
            assert!(before.commits.split_restores.is_empty());
            let report = tx.local_nodes()?.into_iter().find(|node| node.shape.name == "report.txt").unwrap();
            let mut after = before.clone();
            after.commits.trash_groups.insert(report.node_id.clone(), "0190a6b2-0000-7000-8000-000000000001:3".into());
            let split = koidrive_engine::restore::SplitRestore {
                trash_group: "0190a6b2-0000-7000-8000-000000000003:0".into(),
                parent: report.shape.parent_id.clone(),
                name: "report.txt".into(),
                folders: ["0190a6b2-0000-7000-8000-000000000004".into()].into(),
                landed: None,
            };
            after.commits.split_restores.insert("0190a6b2-0000-7000-8000-000000000002".into(), split);
            tx.update_engine_state(&before, &after)?;
            assert_eq!(tx.engine_state()?, after);
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
