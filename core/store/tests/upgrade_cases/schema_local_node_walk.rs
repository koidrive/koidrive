// SPDX-License-Identifier: Apache-2.0

upgrade_case!(schema_local_node_walk, |store| {
    store
        .transaction(|tx| {
            let nodes = tx.local_nodes()?;
            assert!(!nodes.is_empty());
            let mut walked = Vec::new();
            let stopped = tx.each_local_node(|node| {
                walked.push(node);
                std::ops::ControlFlow::<()>::Continue(())
            })?;
            assert_eq!((stopped, &walked), (None, &nodes));
            let first = tx.each_local_node(std::ops::ControlFlow::Break)?;
            assert_eq!(first.as_ref(), nodes.first());
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
