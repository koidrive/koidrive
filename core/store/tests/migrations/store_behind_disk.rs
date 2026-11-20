// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_recovery_stored_before_it_kept_the_store_behind_the_disk_keeps_it_when_the_disk_caused_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    for (trigger, behind) in [
        (RecoveryTrigger::StoreOlderThanDisk, true),
        (RecoveryTrigger::SpaceRewound, false),
    ] {
        let mut state = engine_state();
        state.commits.halted = Some(Halt::Recovery {
            cause: RecoveryCause::of(trigger),
            policy: None,
            store_behind_disk: behind,
        });
        Store::open(&path)
            .unwrap()
            .transaction(|tx| tx.set_engine_state(&state))
            .unwrap();
        store_before(&path, "store_behind_disk");

        let mut store = Store::open(&path).unwrap();
        assert_eq!(applied(&path), all_migrations());
        assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), state);
    }
}
