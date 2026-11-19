// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_store_that_recorded_pending_names_opens_owing_each_rename_r_and_s_still_hold() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let live = |node_id: &str, name: &str| NodeState {
        node_id: node_id.into(),
        parent_id: "root".into(),
        name: name.into(),
        kind: NodeKind::Dir as i32,
        status: NodeStatus::Live as i32,
        content_rev: 2,
        meta_rev: 2,
        ..Default::default()
    };
    let trashed = |node_id: &str, name: &str| NodeState {
        status: NodeStatus::Trashed as i32,
        trash_group: "t".into(),
        ..live(node_id, name)
    };
    let synced = |node_id: &str, name: &str| SyncedNode {
        node_id: node_id.into(),
        shape: dir_shape("root", name),
        pending_for: String::new(),
    };
    let remote = [
        live("d", "A (pending)"),
        live("e", "B"),
        trashed("g", "C (pending)"),
    ];
    let held = [
        synced("d", "A (pending)"),
        synced("e", "B (pending)"),
        synced("g", "C (pending)"),
    ];
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            for node in &remote {
                tx.put_remote_node(node)?;
            }
            for node in &held {
                tx.put_synced_node(node)?;
            }
            Ok::<_, Error>(())
        })
        .unwrap();
    store_before(&path, "pending_marks");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "INSERT INTO pending_names VALUES
             ('d', 'A (pending)', 'A', 1), ('e', 'B (pending)', 'B', 1),
             ('g', 'C (pending)', 'C', 1), ('h', 'D (pending)', 'D', 0);",
    )
    .unwrap();
    drop(conn);

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    let recorded: i64 = Connection::open(&path)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name = 'pending_names'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(recorded, 0);
    assert_eq!(store.transaction(|tx| tx.remote_nodes()).unwrap(), remote);
    assert_eq!(store.transaction(|tx| tx.synced_nodes()).unwrap(), held);
    let mut state = EngineState::default();
    state.trees.pending_finishes.insert(
        "d".into(),
        PendingFinish {
            parent: "root".into(),
            pending: "A (pending)".into(),
            name: "A".into(),
            origin: FinishOrigin::Legacy,
            disk_first: false,
            base: None,
            sending: false,
        },
    );
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), state);
    state.trees.unsent_marks.insert("f".into(), "C".into());
    state.commits.in_flight = Some(InFlight {
        restores: Default::default(),
        owners: Default::default(),
        commit: Commit::default(),
        deletes: BTreeMap::new(),
        result: None,
    });
    store.transaction(|tx| tx.set_engine_state(&state)).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), state);
    state.trees.pending_finishes.clear();
    store.transaction(|tx| tx.set_engine_state(&state)).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), state);
}

#[test]
fn a_rename_a_store_recorded_before_pending_marks_is_finished_after_the_upgrade_though_another_node_holds_its_name()
 {
    use koidrive_engine::classify::Trees;
    use koidrive_engine::ids::IdGen;
    use koidrive_engine::tree::RemoteNode;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let at = |name: &str| EngineShape {
        parent: "root".into(),
        name: name.into(),
        kind: Kind::Dir,
        content: None,
    };
    let held = [("d", "A (pending)"), ("x", "A")];
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            for (node_id, name) in held {
                tx.put_remote_node(&NodeState {
                    node_id: node_id.into(),
                    parent_id: "root".into(),
                    name: name.into(),
                    kind: NodeKind::Dir as i32,
                    status: NodeStatus::Live as i32,
                    content_rev: 2,
                    meta_rev: 2,
                    ..Default::default()
                })?;
                tx.put_synced_node(&SyncedNode {
                    node_id: node_id.into(),
                    shape: dir_shape("root", name),
                    pending_for: String::new(),
                })?;
            }
            Ok::<_, Error>(())
        })
        .unwrap();
    store_before(&path, "pending_marks");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("INSERT INTO pending_names VALUES ('d', 'A (pending)', 'A', 1);")
        .unwrap();
    drop(conn);

    let mut store = Store::open(&path).unwrap();
    let (mut trees, mut commits) = store
        .transaction(|tx| {
            let mut trees = Trees::new("root");
            for node in tx.remote_nodes()? {
                let (id, remote) = RemoteNode::from_state(&node).unwrap();
                trees.set_remote(id, remote);
            }
            for node in tx.synced_nodes()? {
                trees.set_synced(node.node_id.clone(), Some(at(&node.shape.name)));
                trees.set_synced_mark(
                    node.node_id,
                    Some(node.pending_for).filter(|mark| !mark.is_empty()),
                );
            }
            for (ino, (node_id, name)) in (1..).zip(held) {
                trees.set_local(
                    node_id.into(),
                    koidrive_engine::tree::LocalNode {
                        displaced: false,
                        left_out_when_placed: false,
                        content_from_server: false,
                        shape: at(name),
                        identity: FileIdentity {
                            ino,
                            birth_ns: Some(0),
                            handle: None,
                        },
                        parked: false,
                    },
                );
            }
            let state = tx.engine_state()?;
            trees.restore(state.trees).unwrap();
            trees.rules_scanned();
            trees.settle(&Default::default());
            Ok::<_, Error>((trees, Commits::from_state(state.commits).unwrap()))
        })
        .unwrap();
    for listed in store.transaction(|tx| tx.remote_nodes()).unwrap() {
        let (id, remote) = RemoteNode::from_state(&listed).unwrap();
        trees.set_remote(id, remote);
    }
    trees.settle(commits.aliases());
    let sent = commits
        .build(
            &mut trees,
            &mut IdGen::new(1),
            1_794_268_740_000,
            "history",
            "d1",
            7,
        )
        .expect("the recorded rename is sent");
    assert!(
        matches!(
            sent.ops.as_slice(),
            [Op { op: Some(op::Op::Move(moved)), .. }] if moved.node_id == "d" && moved.new_name == "A"
        ),
        "{:?}",
        sent.ops
    );
}

#[test]
fn a_store_that_recorded_a_pending_name_its_commit_in_flight_sends_opens_owing_the_rename() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let create = Op {
        op: Some(op::Op::CreateDir(koidrive_proto::v1::CreateDir {
            node_id: "d".into(),
            parent_id: "root".into(),
            name: "A (pending)".into(),
            ..Default::default()
        })),
        ..Default::default()
    };
    let mut state = EngineState::default();
    state.commits.in_flight = Some(InFlight {
        restores: Default::default(),
        owners: Default::default(),
        commit: Commit {
            commit_id: "c1".into(),
            device_seq: 1,
            ops: vec![create],
            ..Default::default()
        },
        deletes: BTreeMap::new(),
        result: None,
    });
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            tx.set_engine_state(&state)?;
            tx.put_local_node(&LocalNode {
                left_out_when_placed: false,
                content_from_server: false,
                node_id: "d".into(),
                shape: dir_shape("root", "A (pending)"),
                identity: FileId::stored(7u64.into(), None, None),
                parked: false,
            })
        })
        .unwrap();
    store_before(&path, "pending_marks");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("INSERT INTO pending_names VALUES ('d', 'A (pending)', 'A', 0);")
        .unwrap();
    drop(conn);

    let mut store = Store::open(&path).unwrap();
    state.trees.pending_finishes.insert(
        "d".into(),
        PendingFinish {
            parent: "root".into(),
            pending: "A (pending)".into(),
            name: "A".into(),
            origin: FinishOrigin::Legacy,
            disk_first: false,
            base: None,
            sending: false,
        },
    );
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), state);
}
