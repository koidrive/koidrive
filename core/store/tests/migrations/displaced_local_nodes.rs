// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_displaced_node_a_release_took_out_of_l_comes_back_into_l_where_s_or_r_holds_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/displaced.db"),
        &path,
    )
    .unwrap();
    let conn = Connection::open(&path).unwrap();
    let id = |name: &str| -> String {
        conn.query_row(
            "SELECT node_id FROM synced_nodes WHERE name = ?1",
            [name],
            |row| row.get(0),
        )
        .unwrap()
    };
    let (folder, file) = (id("source"), id("file"));
    conn.execute("DELETE FROM synced_nodes WHERE node_id = ?1", [&file])
        .unwrap();
    conn.execute(
        "INSERT INTO recorded_nodes (node_id, displaced, ino, birth_ns) VALUES ('gone', 1, 9, 9)",
        [],
    )
    .unwrap();
    conn.execute_batch(
        &std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/migrations/missing_nodes.sql"
        ))
        .unwrap(),
    )
    .unwrap();
    conn.execute("INSERT INTO migrations (name) VALUES ('missing_nodes')", [])
        .unwrap();
    for missing in [file.as_str(), "gone"] {
        conn.execute("INSERT INTO missing_nodes (node_id) VALUES (?1)", [missing])
            .unwrap();
    }
    conn.execute_batch(
        &std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/migrations/recorded_found.sql"
        ))
        .unwrap(),
    )
    .unwrap();
    conn.execute(
        "INSERT INTO migrations (name) VALUES ('recorded_found')",
        [],
    )
    .unwrap();
    let kept: String = conn
        .query_row(
            "SELECT node_id FROM local_nodes WHERE node_id NOT IN (SELECT node_id FROM recorded_nodes)",
            [],
            |row| row.get(0),
        )
        .unwrap();
    conn.execute(
        "INSERT INTO recorded_nodes (node_id, displaced, found) VALUES (?1, 0, CAST('dist/kept' AS BLOB))",
        [&kept],
    )
    .unwrap();
    drop(conn);

    let mut store = Store::open(&path).unwrap();
    store
        .transaction(|tx| {
            let local = tx.local_nodes()?;
            let held = |node: &str| local.iter().find(|held| held.node_id == node).unwrap();
            let synced = tx.synced_nodes()?;
            let folder_synced = synced.iter().find(|node| node.node_id == folder).unwrap();
            assert_eq!(held(&folder).shape, folder_synced.shape);
            let remote = tx.remote_nodes()?;
            let file_remote = remote.iter().find(|node| node.node_id == file).unwrap();
            let shape = &held(&file).shape;
            assert_eq!(
                (&shape.parent_id, &shape.name, shape.content.as_ref()),
                (&file_remote.parent_id, &file_remote.name, None)
            );
            assert!(
                local
                    .iter()
                    .all(|node| !node.parked && node.node_id != "gone")
            );
            let trees = tx.engine_state()?.trees;
            assert_eq!(
                trees.displaced.keys().collect::<BTreeSet<_>>(),
                BTreeSet::from([&folder, &file])
            );
            assert_eq!(trees.missing, BTreeSet::from([file.clone()]));
            assert_eq!(
                trees.found,
                BTreeMap::from([(kept.as_str().into(), "dist/kept".into())])
            );
            Ok::<_, Error>(())
        })
        .unwrap();
}
