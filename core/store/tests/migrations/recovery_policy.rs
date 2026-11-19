// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_recovery_stored_before_it_kept_a_policy_halt_opens_without_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let cause = RecoveryCause::rewind(4, 1_700_000_000_000);
    let mut state = engine_state();
    state.commits.halted = Some(Halt::Recovery {
        cause,
        policy: None,
    });
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&state))
        .unwrap();
    store_before(&path, "recovery_policy");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), state);
    let before = state.clone();
    state.commits.halted = Some(Halt::Recovery {
        cause,
        policy: Some(7),
    });
    store
        .transaction(|tx| tx.update_engine_state(&before, &state))
        .unwrap();
    drop(store);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), state);
}
