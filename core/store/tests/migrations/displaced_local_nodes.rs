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
    let (folder, file) = (folder.into(), file.into());

    let mut store = Store::open(&path).unwrap();
    store
        .transaction(|tx| {
            let local = tx.local_nodes()?;
            let held = |node: &koidrive_engine::tree::NodeId|
                local.iter().find(|held| held.node_id == *node).unwrap();
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

#[test]
fn a_displaced_copy_waiting_for_its_legacy_create_survives_the_upgrade() {
    use koidrive_proto::v1::{Commit, CreateFile, Op, op};
    use prost::Message;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    drop(Store::open(&path).unwrap());
    store_before(&path, "displaced_local_nodes");
    let id = NodeId::from("pending");
    let content = file_shape("root", "pending").content.unwrap();
    let commit = Commit {
        ops: vec![Op {
            op: Some(op::Op::CreateFile(CreateFile {
                node_id: id.to_string(),
                parent_id: "root".into(),
                name: "pending.txt".into(),
                content: Some(content.clone()),
                ..Default::default()
            })),
            ..Default::default()
        }],
        ..Default::default()
    };
    let conn = Connection::open(&path).unwrap();
    conn.execute(
        "INSERT INTO recorded_nodes (node_id, displaced, ino, birth_ns) VALUES (?1, 1, 9, 11)",
        [id.to_string()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO commit_in_flight (id, commit_message) VALUES (1, ?1)",
        [commit.encode_to_vec()],
    )
    .unwrap();
    drop(conn);

    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            let state = tx.engine_state()?;
            assert!(state.trees.displaced_while_creating.contains(&id));
            assert_eq!(state.trees.displaced[&id].ino, 9);
            let local = tx.local_nodes()?.pop().unwrap();
            assert_eq!(local.node_id, id);
            assert_eq!(local.shape.parent_id, "root");
            assert_eq!(local.shape.name, "pending.txt");
            assert_eq!(local.shape.content, Some(content));
            Ok::<_, Error>(())
        })
        .unwrap();
}
