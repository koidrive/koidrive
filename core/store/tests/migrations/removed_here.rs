// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_store_from_before_removals_here_were_recorded_opens_with_none_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = engine_state();
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&before))
        .unwrap();
    store_before(&path, "removed_here");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    let mut after = before.clone();
    let flight = after.commits.in_flight.as_mut().unwrap();
    flight.restores = BTreeMap::from([(2, vec!["r".into(), "r-child".into()])]);
    before.trees.removed_here.clear();
    assert!(!after.trees.removed_here.is_empty());
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
    store
        .transaction(|tx| tx.update_engine_state(&before, &after))
        .unwrap();
    drop(store);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), after);
}
