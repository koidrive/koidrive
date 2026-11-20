// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_recovery_stored_before_it_kept_the_store_behind_the_disk_opens_as_one_that_was() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    for trigger in [
        RecoveryTrigger::StoreOlderThanDisk,
        RecoveryTrigger::SpaceRewound,
        RecoveryTrigger::StoreRolledBack,
    ] {
        let mut state = engine_state();
        let halt = |store_behind_disk| {
            Some(Halt::Recovery {
                cause: RecoveryCause::of(trigger),
                policy: None,
                store_behind_disk,
            })
        };
        state.commits.halted = halt(false);
        Store::open(&path)
            .unwrap()
            .transaction(|tx| tx.set_engine_state(&state))
            .unwrap();
        store_before(&path, "store_behind_disk");

        let mut store = Store::open(&path).unwrap();
        assert_eq!(applied(&path), all_migrations());
        state.commits.halted = halt(true);
        assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), state);
    }
}

#[test]
fn a_waiting_removal_stored_before_it_kept_the_store_behind_the_disk_opens_as_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut state = engine_state();
    let removal = |origin| Waiting {
        shape: None,
        origin,
    };
    state.trees.waiting.insert("n".into(), removal(Origin::Other));
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&state))
        .unwrap();
    store_before(&path, "store_behind_disk");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), state);
    let before = state.clone();
    state.trees.waiting.insert("n".into(), removal(Origin::StoreBehindDisk));
    store
        .transaction(|tx| tx.update_engine_state(&before, &state))
        .unwrap();
    drop(store);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), state);
}
