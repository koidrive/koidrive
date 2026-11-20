// SPDX-License-Identifier: Apache-2.0

upgrade_case!(recorded_found, |store| {
    store
        .transaction(|tx| {
            let node = tx.local_nodes()?.into_iter().next().unwrap().node_id;
            let mut state = tx.engine_state()?;
            assert!(state.trees.found.is_empty());
            state.trees.outside.insert(node.clone());
            state.trees.found.insert(node.clone(), "dist/kept".into());
            tx.set_engine_state(&state)?;
            assert_eq!(
                tx.engine_state()?.trees.found,
                std::collections::BTreeMap::from([(node, std::path::PathBuf::from("dist/kept"))])
            );
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
