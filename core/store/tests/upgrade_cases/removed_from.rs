// SPDX-License-Identifier: Apache-2.0

upgrade_case!(removed_from, |store| {
    store
        .transaction(|tx| {
            assert!(tx.engine_state()?.trees.removed_from.is_empty());
            assert!(tx.cursor()?.unwrap().last_seq > 0);
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
