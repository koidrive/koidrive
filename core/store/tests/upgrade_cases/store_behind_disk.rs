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
            state.commits.halted = Some(halt);
            tx.set_engine_state(&state)?;
            assert_eq!(tx.engine_state()?.commits.halted, Some(halt));
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
