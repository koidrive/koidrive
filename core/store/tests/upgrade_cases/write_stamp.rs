// SPDX-License-Identifier: Apache-2.0

upgrade_case!(write_stamp, |store| {
    let mounts = |at: &str| {
        std::collections::BTreeMap::from([(
            std::path::PathBuf::from(at),
            koidrive_fs::Mount::Unidentified,
        )])
    };
    let stamp = store.read(|tx| tx.write_stamp()).unwrap();
    assert_eq!(stamp, 0);
    store
        .transaction(|tx| tx.set_volume_mounts(&mounts("1")))
        .unwrap();
    assert_eq!(store.read(|tx| tx.write_stamp()).unwrap(), 0);
    store.stamp_writes(stamp);
    store.check_stamp().unwrap();
    store
        .transaction(|tx| tx.set_volume_mounts(&mounts("2")))
        .unwrap();
    store.transaction(|tx| tx.cursor()).unwrap();
    assert_ne!(store.read(|tx| tx.write_stamp()).unwrap(), 0);
    store.check_stamp().unwrap();
});
