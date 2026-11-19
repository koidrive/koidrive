// SPDX-License-Identifier: Apache-2.0

upgrade_case!(schema_node_ids_as_values, |store| {
    store
        .transaction(|tx| {
            let local = tx.local_nodes()?;
            let synced = tx.synced_nodes()?;
            let state = tx.engine_state()?;
            assert!(!local.is_empty() && !synced.is_empty());
            for node in &local {
                tx.put_local_node(node)?;
            }
            for node in &synced {
                tx.put_synced_node(node)?;
            }
            tx.set_engine_state(&state)?;
            assert_eq!(tx.local_nodes()?, local);
            assert_eq!(tx.synced_nodes()?, synced);
            assert_eq!(tx.engine_state()?, state);
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
