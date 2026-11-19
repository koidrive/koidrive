// SPDX-License-Identifier: Apache-2.0

upgrade_case!(missing_nodes, |store| {
    store
        .transaction(|tx| {
            let mut state = tx.engine_state()?;
            assert!(state.trees.missing.is_empty());
            state.trees.missing.insert("node".into());
            tx.set_engine_state(&state)?;
            assert_eq!(
                tx.engine_state()?.trees.missing,
                std::collections::BTreeSet::from(["node".to_owned()])
            );
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
