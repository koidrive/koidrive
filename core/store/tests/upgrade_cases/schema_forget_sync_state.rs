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
    let kept = ["chunk_cache", "link", "migrations", "volume_mounts"];
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
