// SPDX-License-Identifier: Apache-2.0

upgrade_case!(past_positions_trash_group, |store| {
    store
        .transaction(|tx| {
            let mut state = tx.engine_state()?;
            let kept = |rev, trash_group: Option<&str>| koidrive_engine::state::Agreed {
                rev,
                parent: "root".into(),
                name: "a.txt".into(),
                trash_group: trash_group.map(Into::into),
            };
            let past = vec![kept(2, Some("2:0")), kept(3, None)];
            state
                .trees
                .past_positions
                .insert("upgraded".into(), past.clone());
            tx.set_engine_state(&state)?;
            assert_eq!(
                tx.engine_state()?.trees.past_positions.get(&"upgraded".into()),
                Some(&past)
            );
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
