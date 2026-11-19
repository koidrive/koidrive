// SPDX-License-Identifier: Apache-2.0

upgrade_case!(owed_deletes, |store| {
    store
        .transaction(|tx| {
            assert!(tx.owed_deletes()?.is_empty());
            tx.set_owed_delete("node", true)?;
            assert_eq!(
                tx.owed_deletes()?,
                std::collections::BTreeSet::from(["node".to_owned()])
            );
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
