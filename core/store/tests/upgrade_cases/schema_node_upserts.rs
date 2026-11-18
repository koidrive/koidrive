// SPDX-License-Identifier: Apache-2.0

upgrade_case!(schema_node_upserts, |store| {
    fn rename_every_node(store: &mut Store) {
        store
            .transaction(|tx| {
                let mut local = tx.local_nodes()?;
                let mut synced = tx.synced_nodes()?;
                let mut remote = tx.remote_nodes()?;
                for node in &mut local {
                    node.shape.name.push_str(" upserted");
                    node.left_out_when_placed = !node.left_out_when_placed;
                    tx.put_local_node(node)?;
                }
                for node in &mut synced {
                    node.shape.name.push_str(" upserted");
                    tx.put_synced_node(node)?;
                }
                for node in &mut remote {
                    node.name.push_str(" upserted");
                    node.content_rev += 1;
                    tx.put_remote_node(node)?;
                }
                assert_eq!(tx.local_nodes()?, local);
                assert_eq!(tx.synced_nodes()?, synced);
                assert_eq!(tx.remote_nodes()?, remote);
                Ok::<_, koidrive_store::Error>(())
            })
            .unwrap();
    }
    fn rowids(path: &std::path::Path) -> Vec<Vec<(String, i64)>> {
        let conn = rusqlite::Connection::open(path).unwrap();
        ["local_nodes", "synced_nodes", "remote_nodes"]
            .into_iter()
            .map(|table| {
                conn.prepare(&format!("SELECT node_id, rowid FROM {table} ORDER BY node_id"))
                    .unwrap()
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap()
            })
            .collect()
    }
    rename_every_node(&mut store);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/alpha6.db"),
        &path,
    )
    .unwrap();
    let mut copy = Store::open(&path).unwrap();
    let before = rowids(&path);
    assert!(before.iter().all(|rows| !rows.is_empty()));
    rename_every_node(&mut copy);
    assert_eq!(rowids(&path), before);
});
