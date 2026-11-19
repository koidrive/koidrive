// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_waiting_update_stored_before_released_bases_opens_as_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut state = engine_state();
    let base = EngineShape {
        parent: "root".into(),
        name: "report.txt".into(),
        kind: Kind::File,
        content: None,
    };
    let waiting = |origin| Waiting {
        shape: Some(base.clone()),
        origin,
    };
    state.trees.waiting.insert("n".into(), waiting(Origin::Other));
    state.trees.waiting.insert("m".into(), waiting(Origin::Commit(3)));
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&state))
        .unwrap();
    store_before(&path, "waiting_update_released");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), state);
    let before = state.clone();
    state.trees.waiting.insert("n".into(), waiting(Origin::Released));
    store
        .transaction(|tx| tx.update_engine_state(&before, &state))
        .unwrap();
    drop(store);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), state);
}
