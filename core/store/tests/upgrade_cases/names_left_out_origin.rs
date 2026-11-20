// SPDX-License-Identifier: Apache-2.0

upgrade_case!(names_left_out_origin, |store| {
    use koidrive_engine::classify::Trees;
    use koidrive_engine::commit::{Commits, Failure};
    use koidrive_engine::ids::IdGen;
    use koidrive_engine::ignore::{RuleFile, Source};
    use koidrive_engine::state::{NameLeftOut, RenamedOver};
    use koidrive_engine::tree::RemoteNode;
    use koidrive_proto::v1::{Commit, Error, Move, Op, Place, error, op};
    use prost::Message;
    use rusqlite::{Connection, params};

    assert!(
        store
            .transaction(|tx| tx.engine_state())
            .unwrap()
            .trees
            .names_left_out
            .is_empty()
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/alpha6.db"),
        &path,
    )
    .unwrap();
    let conn = Connection::open(&path).unwrap();
    for name in ["recovery_policy", "names_left_out"] {
        let migration = koidrive_store::migrations::MIGRATIONS
            .iter()
            .find(|m| m.name == name)
            .unwrap();
        conn.execute_batch(migration.sql).unwrap();
        conn.execute("INSERT INTO migrations (name) VALUES (?1)", [name])
            .unwrap();
    }
    let (id, parent, name): (String, String, String) = conn
        .query_row(
            "SELECT node_id, parent_id, name FROM remote_nodes WHERE name = 'report.txt'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    let bytes: Vec<u8> = conn
        .query_row(
            "SELECT commit_message FROM commit_in_flight WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let mut flight = Commit::decode(bytes.as_slice()).unwrap();
    flight.ops = vec![Op {
        op: Some(op::Op::Move(Move {
            node_id: id.clone(),
            from: Some(Place {
                parent_id: parent.clone(),
                name: name.clone(),
            }),
            new_parent_id: parent.clone(),
            new_name: "report (conflict from fixture).txt".into(),
            ..Default::default()
        })),
        ..Default::default()
    }];
    conn.execute(
        "UPDATE commit_in_flight SET commit_message = ?1, result = NULL",
        [flight.encode_to_vec()],
    )
    .unwrap();
    conn.execute("DELETE FROM local_nodes WHERE node_id = ?1", [&id])
        .unwrap();
    conn.execute("DELETE FROM synced_nodes WHERE node_id = ?1", [&id])
        .unwrap();
    conn.execute(
        "INSERT INTO names_left_out (node_id, sent_parent_id, sent_name) VALUES (?1, ?2, ?3)",
        params![id, parent, name],
    )
    .unwrap();
    assert!(conn.prepare("SELECT origin FROM names_left_out").is_err());
    let before = preserved_rows(&conn);
    conn.execute_batch(
        "CREATE TRIGGER fail_origin BEFORE INSERT ON migrations
         WHEN NEW.name = 'names_left_out_origin'
         BEGIN SELECT RAISE(ABORT, 'origin migration interrupted'); END;",
    )
    .unwrap();
    drop(conn);
    assert!(Store::open(&path).is_err());
    let conn = Connection::open(&path).unwrap();
    assert!(conn.prepare("SELECT origin FROM names_left_out").is_err());
    assert_eq!(preserved_rows(&conn), before);
    conn.execute_batch("DROP TRIGGER fail_origin").unwrap();
    drop(conn);

    let mut upgraded = Store::open(&path).unwrap();
    let (stored, remote) = upgraded
        .transaction(|tx| {
            Ok::<_, koidrive_store::Error>((
                tx.engine_state()?,
                tx.remote_nodes()?
                    .into_iter()
                    .find(|node| node.node_id == id)
                    .unwrap(),
            ))
        })
        .unwrap();
    assert_eq!(preserved_rows(&Connection::open(&path).unwrap()), before);
    let (id, remote) = RemoteNode::from_state(&remote).unwrap();
    let mut trees = Trees::new(parent.as_str());
    trees.set_remote(id.clone(), remote);
    trees.settle(&Default::default());
    let mut state = trees.state();
    state.names_left_out = stored.trees.names_left_out;
    assert_eq!(
        state.names_left_out[&id],
        NameLeftOut::DiskRefusal(Some(RenamedOver {
            parent: parent.as_str().into(),
            name: name.clone(),
        }))
    );
    trees.restore(state).unwrap();
    trees.set_rules::<&str>(
        &[],
        Source::Koiignore,
        Some(RuleFile::parse(b"report.txt/\n")),
    );
    trees.rules_scanned();
    let mut commits = Commits::from_state(stored.commits).unwrap();
    assert_eq!(commits.in_flight().unwrap().commit, flight);
    let rejected = Error {
        code: error::Code::UnsupportedVersion as i32,
        ..Default::default()
    };
    assert_eq!(
        commits.failed(&mut trees, &rejected).unwrap(),
        Failure::Abandoned
    );
    assert_eq!(
        trees.state().names_left_out[&id],
        NameLeftOut::DiskRefusal(None)
    );
    trees.settle(commits.aliases());
    let resend = commits
        .build(
            &mut trees,
            &mut IdGen::new(1),
            1_794_268_740_000,
            &flight.history_id,
            "fixture-device",
            flight.base_seq,
        )
        .unwrap();
    assert!(
        matches!(resend.ops.as_slice(), [Op { op: Some(op::Op::Move(moved)), .. }]
        if moved.node_id == id && moved.from.as_ref().is_some_and(|from| from.parent_id == parent && from.name == name))
    );
});
