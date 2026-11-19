// SPDX-License-Identifier: Apache-2.0

upgrade_case!(engine_state_dropped, |store| {
    type Rows = Vec<(String, Vec<Vec<rusqlite::types::Value>>)>;
    fn rows(path: &std::path::Path) -> Rows {
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
                let query = format!("SELECT * FROM \"{table}\"");
                let count = conn.prepare(&query).unwrap().column_count();
                let order = (1..=count).map(|column| column.to_string()).collect::<Vec<_>>().join(",");
                let mut stmt = conn.prepare(&format!("{query} ORDER BY {order}")).unwrap();
                let rows = stmt
                    .query_map([], |row| (0..count).map(|column| row.get(column)).collect())
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap();
                (table, rows)
            })
            .collect()
    }
    let applied = |path: &std::path::Path| -> bool {
        rusqlite::Connection::open(path)
            .unwrap()
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM migrations WHERE name = 'engine_state_dropped')",
                [],
                |row| row.get(0),
            )
            .unwrap()
    };
    store.transaction(|tx| tx.engine_state()).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/alpha6.db"),
        &path,
    )
    .unwrap();
    let held = rows(&path).into_iter().find(|(table, _)| table == "engine_state").unwrap().1;
    assert_eq!(held.len(), 1);
    drop(Store::open(&path).unwrap());
    assert!(applied(&path));
    assert!(rows(&path).iter().all(|(table, _)| table != "engine_state"));

    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE engine_state (id INTEGER PRIMARY KEY CHECK (id = 1), id_generator BLOB NOT NULL) STRICT;
         DELETE FROM migrations WHERE name = 'engine_state_dropped';",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO engine_state (id, id_generator) VALUES (?1, ?2)",
        rusqlite::params_from_iter(&held[0]),
    )
    .unwrap();
    drop(conn);
    let others = |rows: Rows| -> Rows {
        (rows.into_iter())
            .filter(|(table, _)| table != "engine_state" && table != "migrations")
            .collect()
    };
    let before = others(rows(&path));
    drop(Store::open(&path).unwrap());
    assert!(applied(&path));
    let after = rows(&path);
    assert!(after.iter().all(|(table, _)| table != "engine_state"));
    assert_eq!(others(after.clone()), before);

    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute("DELETE FROM migrations WHERE name = 'engine_state_dropped'", [])
        .unwrap();
    drop(conn);
    drop(Store::open(&path).unwrap());
    assert!(applied(&path));
    assert_eq!(others(rows(&path)), others(after));
});
