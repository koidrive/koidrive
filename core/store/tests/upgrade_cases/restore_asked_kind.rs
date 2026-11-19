// SPDX-License-Identifier: Apache-2.0

upgrade_case!(restore_asked_kind, |store| {
    store
        .transaction(|tx| {
            let id = "0190a6b2-0000-7000-8000-000000000005";
            tx.request_restore(
                id,
                &koidrive_engine::restore::RestoreIntent::from_trash(
                    "0190a6b2-0000-7000-8000-000000000006".into(),
                    None,
                    None,
                )
                .unwrap(),
            )?;
            let asked = koidrive_store::AskedRestore {
                path: Some("photos".into()),
                version_saved_at_ms: None,
                kind: Some(koidrive_engine::tree::Kind::Dir),
                files: Some(2),
            };
            tx.note_asked_restore(id, &asked)?;
            assert_eq!(tx.asked_restores()?.get(id), Some(&asked));
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
