// SPDX-License-Identifier: Apache-2.0

upgrade_case!(schema_failed_upgrade_keeps_release_store, |store| {
    fn state(path: &std::path::Path) -> Vec<(String, String)> {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.prepare(
            "SELECT 'schema', name || ': ' || coalesce(sql, '') FROM sqlite_schema
             UNION ALL SELECT 'migration', name FROM migrations ORDER BY 1, 2",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/alpha6.db"),
        &path,
    )
    .unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    let applied: Vec<String> = (conn.prepare("SELECT name FROM migrations").unwrap())
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let pending: Vec<_> = (koidrive_store::migrations::MIGRATIONS.iter())
        .filter(|m| !applied.iter().any(|name| name == m.name))
        .collect();
    assert!(pending.len() > 1);
    conn.execute_batch(&format!(
        "CREATE TRIGGER failing_upgrade BEFORE INSERT ON migrations WHEN NEW.name = '{}'
         BEGIN SELECT RAISE(ABORT, 'the last migration fails'); END;",
        pending.last().unwrap().name
    ))
    .unwrap();
    drop(conn);
    let before = state(&path);

    assert!(Store::open(&path).is_err());
    assert_eq!(state(&path), before);
});
