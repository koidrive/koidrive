// SPDX-License-Identifier: Apache-2.0

upgrade_case!(commit_slot_given_up, |store| {
    store
        .transaction(|tx| {
            let mut state = tx.engine_state()?;
            assert!(state.commits.given_up.is_empty());
            state
                .commits
                .given_up
                .insert(1, vec!["commit-a".into(), "commit-b".into()]);
            tx.set_engine_state(&state)?;
            assert_eq!(tx.engine_state()?.commits.given_up, state.commits.given_up);
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
