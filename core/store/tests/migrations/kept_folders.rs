// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_store_from_before_kept_folders_were_recorded_opens_with_none_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = engine_state();
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&before))
        .unwrap();
    store_before(&path, "kept_folders");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    let after = before.clone();
    before.trees.kept_folders.clear();
    before.trees.kept_entries.clear();
    before.trees.kept_nodes.clear();
    assert!(!after.trees.kept_folders.is_empty());
    assert!(!after.trees.kept_entries.is_empty());
    assert!(!after.trees.kept_nodes.is_empty());
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
    store
        .transaction(|tx| tx.update_engine_state(&before, &after))
        .unwrap();
    drop(store);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), after);
}
