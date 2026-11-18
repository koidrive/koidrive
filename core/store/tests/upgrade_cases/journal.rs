// SPDX-License-Identifier: Apache-2.0

upgrade_case!(journal, |store| {
    store
        .transaction(|tx| {
            assert_eq!(tx.engine_state()?.trees.journal, Default::default());
            assert!(tx.journal_backfill_owed()?);
            assert!(tx.synced_intents()?.is_empty());
            let remote = tx.remote_nodes()?;
            assert!(!remote.is_empty());
            assert!(remote.iter().all(|node| node.intents.is_none() && node.trash_held.is_empty()));
            let node = tx.synced_nodes()?.remove(0).node_id;
            let intents = koidrive_proto::v1::FieldIntents {
                place: Some(koidrive_proto::v1::Intent {
                    id: vec![7; 16],
                    author_device_id: "d1".into(),
                    ..Default::default()
                }),
                content: None,
                status: None,
            };
            tx.set_synced_intents(&node, Some(&intents))?;
            assert_eq!(
                tx.synced_intents()?,
                std::collections::BTreeMap::from([(node, intents)])
            );
            tx.journal_backfilled()?;
            assert!(!tx.journal_backfill_owed()?);
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
