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
            let mut ctimes = std::collections::BTreeMap::new();
            let walked = tx.each_local_ctime(|node_id, ctime_ns| {
                ctimes.insert(node_id, ctime_ns);
                Continue::<()>(())
            })?;
            assert_eq!((walked, ctimes), (None, tx.local_ctimes()?));
            let mut rows = 0;
            let mut state = koidrive_store::EngineState::default();
            let whole = tx.engine_state_unless(&mut state, &mut || {
                rows += 1;
                false
            })?;
            assert!(whole && rows > 0);
            assert_eq!(state, tx.engine_state()?);
            let mut part = koidrive_store::EngineState::default();
            assert!(!tx.engine_state_unless(&mut part, &mut || true)?);
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
