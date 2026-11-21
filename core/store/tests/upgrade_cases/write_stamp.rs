// SPDX-License-Identifier: Apache-2.0

upgrade_case!(write_stamp, |store| {
    let stamp = store.read(|tx| tx.write_stamp()).unwrap();
    assert_eq!(stamp, 0);
    store
        .transaction(|tx| tx.set_id_generator(&1u64.to_le_bytes()))
        .unwrap();
    assert_eq!(store.read(|tx| tx.write_stamp()).unwrap(), 0);
    store.stamp_writes(stamp);
    store.check_stamp().unwrap();
    store
        .transaction(|tx| tx.set_id_generator(&2u64.to_le_bytes()))
        .unwrap();
    store.transaction(|tx| tx.cursor()).unwrap();
    assert_eq!(store.read(|tx| tx.write_stamp()).unwrap(), 1);
    store.check_stamp().unwrap();
});
