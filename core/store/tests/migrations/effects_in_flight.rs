// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_store_from_before_effects_were_planned_in_batches_keeps_its_effect_in_flight() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let before = engine_state();
    assert_eq!(before.effects.in_flight.len(), 1);
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&before))
        .unwrap();
    store_before(&path, "effects_in_flight");
    let stored: Option<Vec<u8>> = Connection::open(&path)
        .unwrap()
        .query_row("SELECT in_flight FROM effect_state", [], |row| row.get(0))
        .unwrap();
    assert!(stored.is_some());

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
    let mut after = before.clone();
    let mut second = after.effects.in_flight[0].clone();
    second.effect_id = "e2".into();
    second.node = "n2".into();
    after.effects.in_flight.push(second);
    store
        .transaction(|tx| tx.update_engine_state(&before, &after))
        .unwrap();
    drop(store);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), after);
}
