// SPDX-License-Identifier: Apache-2.0

upgrade_case!(schema_effect_counter, |store| {
    let counted = |store: &mut Store| {
        let (counter, state) = store
            .read(|tx| Ok::<_, koidrive_store::Error>((tx.effect_counter()?, tx.engine_state()?)))
            .unwrap();
        let effects = state.effects;
        assert_eq!(
            (counter.cleared, counter.in_flight),
            (effects.cleared, effects.in_flight.len() as u64)
        );
        counter
    };
    let release = counted(&mut store);
    let mut state = store.transaction(|tx| tx.engine_state()).unwrap();
    state.effects.cleared = release.cleared + 3;
    store.transaction(|tx| tx.set_engine_state(&state)).unwrap();
    assert_eq!(counted(&mut store).cleared, release.cleared + 3);
});
