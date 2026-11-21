// SPDX-License-Identifier: Apache-2.0

upgrade_case!(kept_paths, |store| {
    use koidrive_engine::state::{KeptAtPath, KeptNode};
    let kept = std::collections::BTreeMap::from([
        (
            vec!["app".to_owned()],
            KeptAtPath {
                kept_folders: true,
                ..Default::default()
            },
        ),
        (
            vec!["app".to_owned(), ".env".to_owned()],
            KeptAtPath {
                kept_nodes: Some(KeptNode { cause: None }),
                ..Default::default()
            },
        ),
    ]);
    store
        .transaction(|tx| {
            let mut state = tx.engine_state()?;
            state.trees.kept_paths = kept.clone();
            tx.set_engine_state(&state)?;
            assert_eq!(tx.engine_state()?.trees.kept_paths, kept);
            tx.forget_sync_state()?;
            assert!(tx.engine_state()?.trees.kept_paths.is_empty());
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
