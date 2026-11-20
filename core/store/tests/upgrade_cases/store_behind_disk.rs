// SPDX-License-Identifier: Apache-2.0

upgrade_case!(store_behind_disk, |store| {
    store
        .transaction(|tx| {
            let mut state = tx.engine_state()?;
            let halt = koidrive_engine::commit::Halt::Recovery {
                cause: koidrive_engine::question::RecoveryCause::rewind(4, 1_700_000_000_000),
                policy: None,
                store_behind_disk: true,
            };
            let removal = koidrive_engine::classify::Waiting {
                shape: None,
                origin: koidrive_engine::classify::Origin::StoreBehindDisk,
            };
            state.commits.halted = Some(halt);
            state.trees.waiting.insert("n".into(), removal.clone());
            tx.set_engine_state(&state)?;
            let stored = tx.engine_state()?;
            assert_eq!(stored.commits.halted, Some(halt));
            assert_eq!(stored.trees.waiting.get(&"n".into()), Some(&removal));
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
