// SPDX-License-Identifier: Apache-2.0

upgrade_case!(displaced_local_nodes, |store| {
    use koidrive_engine::classify::Trees;
    use koidrive_engine::commit::Commits;
    use koidrive_engine::ids::IdGen;
    use koidrive_engine::tree::{ContentKey, Kind, LocalNode, Shape};
    use koidrive_proto::v1::{VersionInfo, op};
    use koidrive_store::migrations::{MIGRATIONS, register_functions};
    use prost::Message;
    use rusqlite::{Connection, params};

    store
        .transaction(|tx| {
            let node = tx.local_nodes()?[0].node_id.clone();
            let mut state = tx.engine_state()?;
            state.trees.outside.insert(node.clone());
            state.trees.lost.insert(node.clone());
            state.trees.found.insert(node.clone(), "dist/kept".into());
            let identity = koidrive_engine::tree::FileIdentity {
                ino: 9,
                birth_ns: Some(-1),
                handle: None,
            };
            state.trees.displaced.insert(node, identity);
            tx.set_engine_state(&state)?;
            let stored = tx.engine_state()?.trees;
            assert_eq!(
                (stored.outside, stored.lost, stored.found, stored.displaced),
                (
                    state.trees.outside,
                    state.trees.lost,
                    state.trees.found,
                    state.trees.displaced
                )
            );
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/alpha6.db"),
        &path,
    )
    .unwrap();
    let conn = Connection::open(&path).unwrap();
    register_functions(&conn).unwrap();
    for migration in MIGRATIONS
        .iter()
        .take_while(|migration| migration.name != "displaced_local_nodes")
    {
        let added = conn
            .execute(
                "INSERT OR IGNORE INTO migrations (name) VALUES (?1)",
                [migration.name],
            )
            .unwrap();
        if added == 1 {
            conn.execute_batch(migration.sql).unwrap();
        }
    }
    let mut expected = Vec::new();
    for (name, different, incomplete) in [
        ("report.txt", false, false),
        ("deck.txt", true, false),
        ("pending.txt", false, true),
    ] {
        let (id, current): (String, Vec<u8>) = conn
            .query_row(
                "SELECT node_id, current FROM remote_nodes WHERE name = ?1",
                [name],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let mut version = VersionInfo::decode(current.as_slice()).unwrap();
        let content = version.content.clone().unwrap();
        assert!(!content.chunks.is_empty());
        let key_only = ContentKey::try_from(&content).unwrap().content(&[]);
        if different {
            let changed = version.content.as_mut().unwrap();
            changed.mtime_ns += 1;
        }
        if incomplete {
            version.content.as_mut().unwrap().chunks.clear();
        }
        conn.execute(
            "UPDATE synced_nodes SET content = ?1 WHERE node_id = ?2",
            params![key_only.encode_to_vec(), id],
        )
        .unwrap();
        conn.execute(
            "UPDATE remote_nodes SET current = ?1 WHERE node_id = ?2",
            params![version.encode_to_vec(), id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO recorded_nodes (node_id, displaced, ino, ino_high, birth_ns, handle)
             SELECT node_id, 1, ino, ino_high, birth_ns, handle FROM local_nodes WHERE node_id = ?1",
            [&id],
        )
        .unwrap();
        conn.execute("DELETE FROM local_nodes WHERE node_id = ?1", [&id])
            .unwrap();
        expected.push((id, (!different && !incomplete).then_some(content)));
    }
    drop(conn);

    let mut upgraded = Store::open(&path).unwrap();
    let (rows, displaced) = upgraded
        .transaction(|tx| Ok::<_, koidrive_store::Error>((tx.local_nodes()?, tx.engine_state()?.trees.displaced)))
        .unwrap();
    for (id, expected) in expected {
        let row = rows.iter().find(|row| row.node_id == id).unwrap();
        assert_eq!(row.shape.content, expected, "{}", row.shape.name);
        let Some(content) = row.shape.content.clone() else {
            continue;
        };
        let (key, chunks) = ContentKey::split(content.clone()).unwrap();
        let identity = displaced[&row.node_id];
        let mut trees = Trees::new(row.shape.parent_id.as_str());
        trees.set_local(
            row.node_id.clone(),
            LocalNode {
                left_out_when_placed: false,
                content_from_server: false,
                shape: Shape {
                    parent: row.shape.parent_id.as_str().into(),
                    name: row.shape.name.clone(),
                    kind: Kind::File,
                    content: Some(key),
                },
                identity,
                parked: row.parked,
                chunks,
            },
        );
        let mut state = trees.state();
        state.displaced.insert(row.node_id.clone(), identity);
        trees.restore(state).unwrap();
        assert!(trees.local().get(&row.node_id).is_none());
        let mut observed = trees.local_entry(&row.node_id).unwrap().clone();
        observed.shape.name = "recovered.txt".into();
        trees.set_local_changed(row.node_id.clone(), observed);
        trees.rules_scanned();
        let mut commits = Commits::default();
        let mut ids = IdGen::new(1);
        let sent = commits
            .build(&mut trees, &mut ids, 1_794_268_740_000, "history", "device", 1)
            .unwrap();
        assert!(matches!(
            sent.ops.as_slice(),
            [operation] if matches!(&operation.op, Some(op::Op::CreateFile(create))
                if create.node_id == id && create.name == "recovered.txt" && create.content.as_ref() == Some(&content))
        ));
    }
});
