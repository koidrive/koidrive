// SPDX-License-Identifier: Apache-2.0

upgrade_case!(unseen_changes, |store| {
    store
        .transaction(|tx| {
            assert!(tx.engine_state()?.trees.unseen_changes.is_empty());
            assert_eq!(tx.cursor()?.unwrap().last_seq, 42);
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
