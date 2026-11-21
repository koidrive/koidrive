// SPDX-License-Identifier: Apache-2.0

upgrade_case!(schema_forget_sync_state_keeps_policy_halt, |store| {
    use koidrive_engine::commit::Halt;
    let cause = koidrive_engine::question::RecoveryCause::rewind(4, 1_700_000_000_000);
    let policy = Some(Halt::Policy { code: 7 });
    for (halted, kept) in [
        (Halt::Recovery { cause, policy: Some(7) }, policy),
        (Halt::Policy { code: 7 }, policy),
        (Halt::ReadOnly, None),
    ] {
        store
            .transaction(|tx| {
                let mut state = tx.engine_state()?;
                state.commits.halted = Some(halted);
                tx.set_engine_state(&state)?;
                tx.forget_sync_state()?;
                let mut forgotten = koidrive_store::EngineState::default();
                forgotten.commits.halted = kept;
                assert_eq!(tx.engine_state()?, forgotten, "{halted:?}");
                Ok::<_, koidrive_store::Error>(())
            })
            .unwrap();
    }
});
