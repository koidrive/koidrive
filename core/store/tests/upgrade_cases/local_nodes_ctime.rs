// SPDX-License-Identifier: Apache-2.0

upgrade_case!(local_nodes_ctime, |store| {
    assert!(store.written_ns().is_some());
    store
        .transaction(|tx| {
            assert!(tx.local_ctimes()?.is_empty());
            let node = tx.local_nodes()?.remove(0);
            tx.set_local_ctime(&node.node_id, Some(1_794_989_910_322_303_152))?;
            tx.put_local_node(&node)?;
            assert_eq!(
                tx.local_ctimes()?,
                std::collections::BTreeMap::from([(node.node_id.clone(), 1_794_989_910_322_303_152)])
            );
            tx.set_local_ctime(&node.node_id, None)?;
            assert!(tx.local_ctimes()?.is_empty());
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
