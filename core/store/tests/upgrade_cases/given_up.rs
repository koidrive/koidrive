// SPDX-License-Identifier: Apache-2.0

upgrade_case!(given_up, |store| {
    store
        .transaction(|tx| {
            let before = tx.engine_state()?;
            assert!(before.commits.given_up_nodes.is_empty());
            let report = tx.local_nodes()?.into_iter().find(|node| node.shape.name == "report.txt").unwrap();
            let mut after = before.clone();
            after.commits.given_up_nodes.insert(
                report.node_id,
                koidrive_engine::state::GivenUp {
                    revisions: koidrive_engine::question::Revisions { content_rev: 3, meta_rev: 7 },
                    held_here: true,
                },
            );
            tx.update_engine_state(&before, &after)?;
            assert_eq!(tx.engine_state()?, after);
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
