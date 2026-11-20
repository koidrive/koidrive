// SPDX-License-Identifier: Apache-2.0

upgrade_case!(local_nodes_ctime, |store| {
    store
        .transaction(|tx| {
            let nodes = tx.local_nodes()?;
            assert!(!nodes.is_empty());
            assert!(tx.local_ctimes()?.is_empty());
            let ctimes: std::collections::BTreeMap<_, _> = nodes
                .into_iter()
                .zip(1_i64..)
                .map(|(node, at)| (node.node_id.to_string(), at))
                .collect();
            for (id, &at) in &ctimes {
                tx.set_local_ctime(id, at)?;
            }
            assert_eq!(tx.local_ctimes()?, ctimes);
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
