// SPDX-License-Identifier: Apache-2.0

upgrade_case!(schema_node_walks, |store| {
    use std::ops::ControlFlow::{Break, Continue};
    fn all<T>(walk: impl FnOnce(&mut dyn FnMut(T) -> std::ops::ControlFlow<()>)) -> Vec<T> {
        let mut nodes = Vec::new();
        walk(&mut |node| {
            nodes.push(node);
            Continue(())
        });
        nodes
    }
    store
        .transaction(|tx| {
            let (remote, synced, local) =
                (tx.remote_nodes()?, tx.synced_nodes()?, tx.local_nodes()?);
            assert!(!remote.is_empty() && !synced.is_empty() && !local.is_empty());
            let walked = (
                all(|each| assert_eq!(tx.each_remote_node(each).unwrap(), None)),
                all(|each| assert_eq!(tx.each_synced_node(each).unwrap(), None)),
                all(|each| assert_eq!(tx.each_local_node(each).unwrap(), None)),
            );
            assert_eq!(walked, (remote.clone(), synced.clone(), local.clone()));
            assert_eq!(tx.each_remote_node(Break)?.as_ref(), remote.first());
            assert_eq!(tx.each_synced_node(Break)?.as_ref(), synced.first());
            assert_eq!(tx.each_local_node(Break)?.as_ref(), local.first());
            let mut rows = 0;
            let state = tx.engine_state_unless(&mut || {
                rows += 1;
                false
            })?;
            assert!(rows > 0);
            assert_eq!(state, Some(tx.engine_state()?));
            assert_eq!(tx.engine_state_unless(&mut || true)?, None);
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
