// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_store_from_before_trash_groups_reads_each_kept_position_as_a_live_record() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let live = Agreed {
        rev: 3,
        parent: "root".into(),
        name: "a.txt".into(),
        trash_group: None,
    };
    let mut state = EngineState::default();
    state.trees.past_positions = BTreeMap::from([("n".into(), vec![live.clone()])]);
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&state))
        .unwrap();
    store_before(&path, "past_positions_trash_group");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    let read = store.transaction(|tx| tx.engine_state()).unwrap();
    assert_eq!(read.trees.past_positions, state.trees.past_positions);
    let trashed = Agreed {
        rev: 2,
        trash_group: Some("2:0".into()),
        ..live.clone()
    };
    state.trees.past_positions = BTreeMap::from([("n".into(), vec![trashed, live])]);
    store.transaction(|tx| tx.set_engine_state(&state)).unwrap();
    let read = store.transaction(|tx| tx.engine_state()).unwrap();
    assert_eq!(read.trees.past_positions, state.trees.past_positions);
}
