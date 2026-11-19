// SPDX-License-Identifier: Apache-2.0

upgrade_case!(unread_folders, |store| {
    store
        .transaction(|tx| {
            assert_eq!(tx.unread_folders()?, None);
            let unread = std::collections::BTreeMap::from([
                (std::path::PathBuf::from("Private"), 1_795_079_591_616_636_478),
                (std::path::PathBuf::from("a/b/Locked"), -1),
            ]);
            tx.set_unread_folders(&unread)?;
            assert_eq!(tx.unread_folders()?, Some(unread));
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
