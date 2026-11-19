// SPDX-License-Identifier: Apache-2.0

upgrade_case!(schema_set_engine_state_replaces_stored, |store| {
    store
        .transaction(|tx| {
            let upgraded = tx.engine_state()?;
            assert_ne!(upgraded, koidrive_store::EngineState::default());
            tx.set_engine_state(&koidrive_store::EngineState::default())?;
            assert_eq!(tx.engine_state()?, koidrive_store::EngineState::default());
            tx.set_engine_state(&upgraded)?;
            assert_eq!(tx.engine_state()?, upgraded);
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
