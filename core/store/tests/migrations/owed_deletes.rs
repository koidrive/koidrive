// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_store_from_before_owed_deletes_opens_with_none_owed_and_keeps_those_owed_after() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    drop(Store::open(&path).unwrap());
    store_before(&path, "owed_deletes");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    assert_eq!(
        store.transaction(|tx| tx.owed_deletes()).unwrap(),
        BTreeSet::new()
    );
    store
        .transaction(|tx| {
            tx.set_owed_delete("a", true)?;
            tx.set_owed_delete("b", true)?;
            tx.set_owed_delete("a", false)
        })
        .unwrap();
    drop(store);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(
        store.transaction(|tx| tx.owed_deletes()).unwrap(),
        BTreeSet::from(["b".to_owned()])
    );
}
