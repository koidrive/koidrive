// SPDX-License-Identifier: Apache-2.0

upgrade_case!(synced_positions, |store| {
    store
        .transaction(|tx| {
            let state = tx.engine_state()?;
            assert!(state.trees.synced_positions.is_empty());
            assert!(!state.trees.past_positions.is_empty());
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
