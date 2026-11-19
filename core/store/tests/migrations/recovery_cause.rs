// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_recovery_stored_before_its_cause_was_opens_as_a_rolled_back_store() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut state = engine_state();
    state.commits.halted = Some(Halt::Recovery { cause: RecoveryCause::rewind(4, 1_700_000_000_000), policy: None });
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&state))
        .unwrap();
    store_before(&path, "recovery_cause");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    state.commits.halted = Some(Halt::Recovery { cause: RecoveryCause::of(
        RecoveryTrigger::StoreRolledBack,
    ), policy: None });
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), state);
}
