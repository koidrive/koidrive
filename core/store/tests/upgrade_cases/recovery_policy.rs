// SPDX-License-Identifier: Apache-2.0

upgrade_case!(recovery_policy, |store| {
    store
        .transaction(|tx| {
            let mut state = tx.engine_state()?;
            let halt = koidrive_engine::commit::Halt::Recovery {
                cause: koidrive_engine::question::RecoveryCause::rewind(4, 1_700_000_000_000),
                policy: Some(7),
            };
            state.commits.halted = Some(halt);
            tx.set_engine_state(&state)?;
            assert_eq!(tx.engine_state()?.commits.halted, Some(halt));
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
