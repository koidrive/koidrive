// SPDX-License-Identifier: Apache-2.0

upgrade_case!(local_nodes_ctime, |store| {
    store
        .transaction(|tx| {
            let ctimes = tx.local_ctimes()?;
            assert_eq!(ctimes.len(), tx.local_nodes()?.len());
            let bound = ctimes.values().next().copied();
            assert!(ctimes.values().all(|at| Some(*at) == bound));
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
