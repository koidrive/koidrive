// SPDX-License-Identifier: Apache-2.0

upgrade_case!(schema_forget_sync_state, |store| {
    fn rows(path: &std::path::Path) -> Vec<(String, i64)> {
        let conn = rusqlite::Connection::open(path).unwrap();
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        (tables.into_iter())
            .map(|table| {
                let count = conn
                    .query_row(&format!("SELECT count(*) FROM \"{table}\""), [], |row| row.get(0))
                    .unwrap();
                (table, count)
            })
            .collect()
    }
    store.transaction(|tx| tx.forget_sync_state()).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/alpha6.db"),
        &path,
    )
    .unwrap();
    let mut copy = Store::open(&path).unwrap();
    let tokens = koidrive_fs::RootTokens {
        current: Some("a".repeat(32)),
        pending: Some("b".repeat(32)),
    };
    let mounts = std::collections::BTreeMap::from([(
        std::path::PathBuf::from("Disk"),
        koidrive_fs::Mount::Unidentified,
    )]);
    copy.transaction(|tx| {
        tx.set_root_tokens(&tokens)?;
        tx.set_volume_mounts(&mounts)?;
        tx.set_id_generator(&7u64.to_le_bytes())
    })
    .unwrap();
    let before = rows(&path);
    copy.transaction(|tx| tx.forget_sync_state()).unwrap();
    copy.transaction(|tx| {
        assert_eq!(tx.root_tokens()?, tokens);
        assert_eq!(tx.volume_mounts()?, mounts);
        assert_eq!(tx.id_generator()?, Some(7u64.to_le_bytes().to_vec()));
        assert_eq!(tx.cursor()?, None);
        assert_eq!(tx.engine_state()?, koidrive_store::EngineState::default());
        Ok::<_, koidrive_store::Error>(())
    })
    .unwrap();
    let kept = [
        "chunk_cache",
        "link",
        "migrations",
        "restore_asked",
        "restore_intents",
        "restore_requests",
        "volume_mounts",
    ];
    let forgotten = "commit_in_flight commit_in_flight_delete_members commit_in_flight_deletes \
         commit_in_flight_restore_members commit_slots commit_state content_provenance cycle_reverted \
         discarded_nodes dropped_edits edited_here effect_state effects_in_flight engine_state failed_effects \
         forgotten_shapes guard_confirmed guard_confirmed_limits guard_confirmed_members guard_ledger \
         guard_remote guard_remote_dismissed kept_entries kept_folders kept_nodes kept_originals local_changes local_nodes lost_creates \
         lost_positions lost_shapes moved_here node_maps node_sets open_question_items open_question_members \
         open_question_shapes open_questions past_positions pending_finishes published_nodes questions \
         recorded_nodes remote_cursor remote_nodes removed_here replaced_versions resends synced_nodes \
         taken_answers trashed_here unseen_changes unsent_creates unsent_edits unsent_marks unsent_moves waiting_updates \
         watched_contents watched_positions";
    let mut tables: Vec<&str> = kept.iter().copied().chain(forgotten.split_whitespace()).collect();
    tables.sort();
    let found: Vec<&str> = before.iter().map(|(table, _)| table.as_str()).collect();
    assert_eq!(found, tables);
    let emptied = (before.iter()).filter(|(table, count)| *count > 0 && !kept.contains(&table.as_str()));
    assert!(emptied.count() > 10, "{before:?}");
    for ((table, was), (_, now)) in before.iter().zip(rows(&path)) {
        let expected = match table.as_str() {
            "engine_state" => 1,
            table if kept.contains(&table) => *was,
            _ => 0,
        };
        assert_eq!(now, expected, "{table}");
    }
});
