// SPDX-License-Identifier: Apache-2.0

upgrade_case!(schema_remote_nodes_without_intents, |store| {
    store
        .transaction(|tx| {
            let remote = tx.remote_nodes()?;
            assert!(!remote.is_empty());
            assert!(
                remote
                    .iter()
                    .all(|node| node.intents.is_none() && node.trash_held.is_empty())
            );
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
