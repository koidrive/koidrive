// SPDX-License-Identifier: Apache-2.0

upgrade_case!(unsent_deletes, |store| {
    let before = store.transaction(|tx| tx.engine_state()).unwrap();
    assert!(before.trees.unsent_deletes.is_empty());
    let mut after = before.clone();
    after.trees.unsent_deletes.insert(
        "0190a6b2-0000-7000-8000-000000000009".into(),
        koidrive_engine::tree::Shape {
            parent: "root".into(),
            name: "deleted.txt".into(),
            kind: koidrive_engine::tree::Kind::File,
            content: None,
        },
    );
    store
        .transaction(|tx| tx.update_engine_state(&before, &after))
        .unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), after);
});
