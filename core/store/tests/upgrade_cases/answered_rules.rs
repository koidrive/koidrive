// SPDX-License-Identifier: Apache-2.0

upgrade_case!(answered_rules, |store| {
    store
        .transaction(|tx| {
            let mut state = tx.engine_state()?;
            assert!(state.trees.answered_rules.is_empty());
            let answered = koidrive_engine::state::AnsweredRules {
                folder: "folder".into(),
                gitignore: false,
                rules: b".env\n".to_vec(),
            };
            state.trees.answered_rules.insert("rules".into(), answered);
            tx.set_engine_state(&state)?;
            assert_eq!(tx.engine_state()?.trees.answered_rules, state.trees.answered_rules);
            tx.forget_sync_state()?;
            assert!(tx.engine_state()?.trees.answered_rules.is_empty());
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
