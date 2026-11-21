// SPDX-License-Identifier: Apache-2.0

upgrade_case!(born_as_temp, |store| {
    store
        .transaction(|tx| {
            let mut state = tx.engine_state()?;
            assert!(state.trees.born_as_temp.is_empty());
            let report = tx
                .local_nodes()?
                .into_iter()
                .find(|node| node.shape.name == "report.txt")
                .unwrap();
            (state.trees.born_as_temp).insert(
                report.node_id.clone(),
                koidrive_engine::state::TempBirth::Upgraded,
            );
            tx.set_engine_state(&state)?;
            assert_eq!(
                tx.engine_state()?.trees.born_as_temp,
                std::collections::BTreeMap::from([(
                    report.node_id,
                    koidrive_engine::state::TempBirth::Upgraded
                )])
            );
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
