// SPDX-License-Identifier: Apache-2.0

upgrade_case!(file_handles, |store| {
    store
        .transaction(|tx| {
            let mut node = tx.local_nodes()?.into_iter().next().unwrap();
            assert!(tx.local_nodes()?.iter().all(|node| node.identity.handle.is_none()));
            node.identity.handle = Some(u64::MAX);
            tx.put_local_node(&node)?;
            let read = tx.local_nodes()?;
            let back = read.iter().find(|read| read.node_id == node.node_id).unwrap();
            assert_eq!(back.identity, node.identity);
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
