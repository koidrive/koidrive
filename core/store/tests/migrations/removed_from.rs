// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_store_from_before_removals_were_recorded_opens_with_none_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = engine_state();
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&before))
        .unwrap();
    store_before(&path, "removed_from");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    let after = before.clone();
    before.trees.removed_from.clear();
    before.commits.trash_groups.clear();
    assert!(!after.trees.removed_from.is_empty());
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
    store
        .transaction(|tx| tx.update_engine_state(&before, &after))
        .unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), after);
}
